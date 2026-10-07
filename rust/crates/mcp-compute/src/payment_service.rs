//! Durable policy storage and trusted wallet resolution.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::get};
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::driver::{ComputeError, Result};
use crate::google::GoogleCloudFunctionsDriver;
use crate::payment_policy::{
    Allocation, DeploymentIdentity, OwnedWallet, PaymentPolicy, PaymentPolicySpec, revision,
};
use crate::payment_repository::{PolicyRepository, StoredPolicy};
use crate::types::Tenant;

#[derive(Clone)]
pub struct PaymentService {
    google: GoogleCloudFunctionsDriver,
    repository: PolicyRepository,
    wallet_url: String,
    wallet_proof: String,
    client: reqwest::Client,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetPaymentPolicyRequest {
    pub hostname: String,
    /// Zero creates a policy; updates must supply the last observed version.
    pub expected_version: u64,
    pub policy: PaymentPolicySpec,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetPaymentPolicyRequest {
    pub hostname: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeletePaymentPolicyRequest {
    pub hostname: String,
    pub expected_version: u64,
}

#[derive(Serialize)]
pub struct ResolvedPaymentPolicy {
    pub deployment: DeploymentIdentity,
    pub version: u64,
    pub price_micro_usd: u64,
    pub expires_at: u64,
    pub schemes: Vec<String>,
    pub allocations: Vec<Allocation>,
}

impl PaymentService {
    #[cfg(test)]
    pub(crate) fn for_test(google: GoogleCloudFunctionsDriver, wallet_url: String) -> Self {
        Self {
            repository: google.policy_repository().unwrap().clone(),
            google,
            wallet_url,
            wallet_proof: "fixture-wallet-proof".into(),
            client: reqwest::Client::new(),
        }
    }

    pub fn from_env(google: GoogleCloudFunctionsDriver) -> Result<Option<Self>> {
        let Some(repository) = google.policy_repository().cloned() else {
            return Ok(None);
        };
        let wallet_url = std::env::var("COMPUTE_WALLET_SERVICE_URL")
            .map_err(|_| configuration("COMPUTE_WALLET_SERVICE_URL is required"))?;
        let url = url::Url::parse(&wallet_url)
            .map_err(|_| configuration("invalid wallet service URL"))?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err(configuration("wallet service URL must be an HTTPS origin"));
        }
        let wallet_proof = std::env::var("COMPUTE_WALLET_RESOLVER_PROOF")
            .map_err(|_| configuration("COMPUTE_WALLET_RESOLVER_PROOF is required"))?;
        if wallet_proof.len() < 32 {
            return Err(configuration(
                "wallet resolver proof must contain at least 32 bytes",
            ));
        }
        Ok(Some(Self {
            repository,
            google,
            wallet_url: wallet_url.trim_end_matches('/').into(),
            wallet_proof,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        }))
    }

    async fn load(&self, deployment: &DeploymentIdentity) -> Result<Option<StoredPolicy>> {
        self.repository
            .load(&self.google.access_token().await?, deployment)
            .await
    }

    async fn wallets(
        &self,
        deployment: &DeploymentIdentity,
        spec: &PaymentPolicySpec,
    ) -> Result<Vec<OwnedWallet>> {
        let wallets: Vec<_> = std::iter::once(&spec.primary_recipient)
            .chain(spec.splits.iter().map(|split| &split.recipient))
            .collect();
        let token = self
            .google
            .identity_token(&self.wallet_url)
            .await?
            .ok_or_else(|| configuration("wallet resolution requires service identity"))?;
        let response = self
            .client
            .post(format!("{}/__402/resolve-recipients", self.wallet_url))
            .bearer_auth(token)
            .header("x-pay-wallet-resolver-proof", &self.wallet_proof)
            .json(&json!({"owner_key": deployment.owner_key, "wallets": wallets}))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ComputeError::InvalidRequest(
                "recipient wallet ownership could not be verified".into(),
            ));
        }
        Ok(response.json().await?)
    }

    pub async fn get(&self, tenant: &Tenant, hostname: &str) -> Result<PaymentPolicy> {
        let deployment = self.google.payment_deployment(hostname, None).await?;
        require_owner(tenant, &deployment)?;
        let stored = self.load(&deployment).await?.ok_or_else(|| {
            ComputeError::InvalidRequest("deployment has no payment policy".into())
        })?;
        stored.policy.ok_or_else(|| {
            ComputeError::InvalidRequest("deployment payment policy is retired".into())
        })
    }

    pub async fn set(
        &self,
        tenant: &Tenant,
        request: SetPaymentPolicyRequest,
    ) -> Result<PaymentPolicy> {
        let deployment = self
            .google
            .payment_deployment(&request.hostname, None)
            .await?;
        require_owner(tenant, &deployment)?;
        request.policy.validate(now()?)?;
        let stored = self.load(&deployment).await?;
        if stored.as_ref().is_some_and(|stored| stored.retired) {
            return Err(ComputeError::InvalidRequest(
                "deployment payment policy is permanently retired".into(),
            ));
        }
        let next = revision(
            stored.as_ref().and_then(|value| value.policy.as_ref()),
            deployment.clone(),
            request.policy,
            request.expected_version,
            now()?,
        )?;
        let wallets = self.wallets(&deployment, &next.spec).await?;
        next.resolve(&deployment, &wallets, now()?)?;
        if stored
            .as_ref()
            .is_some_and(|stored| stored.policy.as_ref() == Some(&next))
        {
            return Ok(next);
        }
        self.repository
            .write(
                &self.google.access_token().await?,
                stored.as_ref(),
                &StoredPolicy {
                    deployment,
                    policy: Some(next.clone()),
                    retired: false,
                    absent_since: None,
                    update_time: String::new(),
                },
            )
            .await?;
        Ok(next)
    }

    pub async fn delete(
        &self,
        tenant: &Tenant,
        request: DeletePaymentPolicyRequest,
    ) -> Result<Option<PaymentPolicy>> {
        let deployment = self
            .google
            .payment_deployment(&request.hostname, None)
            .await?;
        require_owner(tenant, &deployment)?;
        let Some(stored) = self.load(&deployment).await? else {
            return Ok(None);
        };
        let Some(policy) = &stored.policy else {
            return Ok(None);
        };
        if policy.deleted {
            return Ok(Some(policy.clone()));
        }
        if policy.version != request.expected_version {
            return Err(ComputeError::InvalidRequest(
                "payment policy version conflict".into(),
            ));
        }
        let mut deleted = policy.clone();
        deleted.deleted = true;
        deleted.version = deleted
            .version
            .checked_add(1)
            .ok_or_else(|| configuration("payment policy version exhausted"))?;
        let mut next = stored.clone();
        next.policy = Some(deleted.clone());
        self.repository
            .write(&self.google.access_token().await?, Some(&stored), &next)
            .await?;
        Ok(Some(deleted))
    }

    pub async fn resolve(&self, hostname: &str, path: &str) -> Result<ResolvedPaymentPolicy> {
        let deployment = self.google.payment_deployment(hostname, Some(path)).await?;
        let stored = self.load(&deployment).await?.ok_or_else(|| {
            ComputeError::InvalidRequest("deployment has no payment policy".into())
        })?;
        let policy = stored.policy.ok_or_else(|| {
            ComputeError::InvalidRequest("deployment payment policy is retired".into())
        })?;
        if policy.deleted || stored.retired {
            return Err(ComputeError::InvalidRequest(
                "payment policy was deleted".into(),
            ));
        }
        policy.spec.validate(now()?)?;
        let wallets = self.wallets(&deployment, &policy.spec).await?;
        let allocations = policy.resolve(&deployment, &wallets, now()?)?;
        Ok(ResolvedPaymentPolicy {
            deployment,
            version: policy.version,
            price_micro_usd: policy.spec.price_micro_usd,
            expires_at: policy.spec.expires_at,
            schemes: policy.spec.schemes,
            allocations,
        })
    }

    pub fn router(self) -> Router {
        Router::new()
            .route("/__402/payment-policy", get(resolve))
            .with_state(self)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveRequest {
    hostname: String,
    path: String,
}

async fn resolve(
    State(service): State<PaymentService>,
    Query(request): Query<ResolveRequest>,
) -> Response {
    match service.resolve(&request.hostname, &request.path).await {
        Ok(policy) => Json(policy).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"payment_policy_unavailable"})),
        )
            .into_response(),
    }
}

fn require_owner(tenant: &Tenant, deployment: &DeploymentIdentity) -> Result<()> {
    if tenant.key != deployment.owner_key {
        return Err(ComputeError::InvalidRequest(
            "deployment belongs to a different payer".into(),
        ));
    }
    Ok(())
}

fn configuration(message: &str) -> ComputeError {
    ComputeError::Configuration(message.into())
}

fn now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .map_err(|_| configuration("system clock precedes Unix epoch"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::google::GoogleConfig;
    use axum::extract::Request;
    use reqwest::Method;
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn durable_policy_is_owned_idempotent_and_resolved_before_invocation() {
        let stored = Arc::new(Mutex::new(None::<Value>));
        let handler_store = stored.clone();
        let patch_error = Arc::new(Mutex::new(None::<(StatusCode, Value)>));
        let handler_patch_error = patch_error.clone();
        let owner = "0123456789abcdef";
        let resource = format!("projects/p/locations/r/functions/gcf-{owner}-sec");
        let hostname = format!("gcf-{owner}-sec.compute.example");
        let resource_response = resource.clone();
        let app = Router::new().fallback(move |request: Request| {
            let stored = handler_store.clone();
            let patch_error = handler_patch_error.clone();
            let resource = resource_response.clone();
            async move {
                let method = request.method().clone();
                let path = request.uri().path().to_owned();
                let query = request.uri().query().unwrap_or("").to_owned();
                if path.starts_with("/v2/") {
                    return Json(json!({
                        "name": resource, "state": "ACTIVE", "createTime": "2026-01-01T00:00:00Z",
                        "labels": {"pay-tenant": owner, "managed-by": "mcp-compute", "pay-exposure": "gateway"},
                        "serviceConfig": {"environmentVariables": {"PAY_INTERNAL_PUBLIC_PATHS": "[\"/summary\"]"}}
                    })).into_response();
                }
                if path == "/__402/resolve-recipients" {
                    assert_eq!(request.headers()["x-pay-wallet-resolver-proof"], "test-wallet-resolver-proof-32-bytes");
                    let body = axum::body::to_bytes(request.into_body(), 4096).await.unwrap();
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(body["owner_key"], owner);
                    let wallets: Vec<_> = body["wallets"].as_array().unwrap().iter().enumerate()
                        .map(|(index, reference)| json!({
                            "owner_key": owner, "reference": reference,
                            "chain": "solana", "address": bs58::encode([index as u8 + 1; 32]).into_string()
                        })).collect();
                    return Json(wallets).into_response();
                }
                if path.starts_with("/policies/") && method == Method::GET {
                    return match stored.lock().unwrap().clone() {
                        Some(value) => Json(value).into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    };
                }
                if path.starts_with("/policies/") && method == Method::PATCH {
                    let body = axum::body::to_bytes(request.into_body(), 16384).await.unwrap();
                    let mut body: Value = serde_json::from_slice(&body).unwrap();
                    let mut stored = stored.lock().unwrap();
                    if stored.is_none() {
                        assert_eq!(query, "currentDocument.exists=false");
                    } else {
                        assert_eq!(query, "currentDocument.updateTime=version1");
                    }
                    if let Some((status, error)) = patch_error.lock().unwrap().take() {
                        return (status, Json(error)).into_response();
                    }
                    body["updateTime"] = json!("version1");
                    *stored = Some(body.clone());
                    return Json(body).into_response();
                }
                panic!("unexpected provider call: {method} {path}");
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let google = GoogleCloudFunctionsDriver::new(GoogleConfig {
            project: "p".into(),
            default_region: "r".into(),
            api_base: base.clone(),
            metadata_base: base.clone(),
            access_token: Some("test-access".into()),
            identity_token: Some("test-identity".into()),
            allow_unauthenticated_invoke: false,
            function_service_account: None,
            build_service_account: None,
            gateway_domain: "compute.example".into(),
            payment_policy_database: None,
        })
        .unwrap();
        let service = PaymentService {
            google,
            repository: PolicyRepository::new(format!("{base}/policies")).unwrap(),
            wallet_url: base,
            wallet_proof: "test-wallet-resolver-proof-32-bytes".into(),
            client: reqwest::Client::new(),
        };
        let tenant = Tenant {
            payer: "verified-payer".into(),
            key: owner.into(),
            channel_id: "channel".into(),
        };
        let reference = |name: &str| crate::payment_policy::WalletReference {
            driver: "privy".into(),
            name: name.into(),
        };
        let request = SetPaymentPolicyRequest {
            hostname: hostname.clone(),
            expected_version: 0,
            policy: PaymentPolicySpec {
                price_micro_usd: 50_000,
                schemes: vec!["mpp-session".into()],
                expires_at: now().unwrap() + 3600,
                primary_recipient: reference("infrastructure"),
                splits: vec![
                    crate::payment_policy::PaymentSplit {
                        recipient: reference("tax"),
                        basis_points: 3000,
                    },
                    crate::payment_policy::PaymentSplit {
                        recipient: reference("profit"),
                        basis_points: 1000,
                    },
                ],
            },
        };
        assert!(service.resolve(&hostname, "/summary").await.is_err());
        let mut other = tenant.clone();
        other.key = "fedcba9876543210".into();
        assert!(service.set(&other, request.clone()).await.is_err());
        assert!(stored.lock().unwrap().is_none());
        let first = service.set(&tenant, request.clone()).await.unwrap();
        assert_eq!(first.version, 1);
        assert_eq!(service.set(&tenant, request.clone()).await.unwrap(), first);
        assert!(service.get(&other, &hostname).await.is_err());
        let resolved = service
            .resolve(&hostname, "/summary?ticker=AAPL")
            .await
            .unwrap();
        assert_eq!(resolved.price_micro_usd, 50_000);
        assert_eq!(
            resolved
                .allocations
                .iter()
                .map(|a| a.amount_micro_usd)
                .collect::<Vec<_>>(),
            [30_000, 15_000, 5_000]
        );
        assert!(
            service
                .resolve(&hostname, "/internal/refresh")
                .await
                .is_err()
        );
        assert!(
            service
                .resolve("gcf-bad.other.example", "/summary")
                .await
                .is_err()
        );
        let mut update = request;
        update.policy.price_micro_usd = 60_000;
        assert!(service.set(&tenant, update.clone()).await.is_err());
        update.expected_version = 1;
        for (http_status, google_status, conflict) in [
            (StatusCode::BAD_REQUEST, "FAILED_PRECONDITION", true),
            (StatusCode::CONFLICT, "ABORTED", true),
            (StatusCode::CONFLICT, "ALREADY_EXISTS", true),
            (StatusCode::PRECONDITION_FAILED, "", true),
            (StatusCode::BAD_REQUEST, "INVALID_ARGUMENT", false),
            (StatusCode::BAD_REQUEST, "", false),
        ] {
            *patch_error.lock().unwrap() = Some((
                http_status,
                json!({"error": {
                    "code": http_status.as_u16(),
                    "message": "private upstream details: FAILED_PRECONDITION",
                    "status": google_status
                }}),
            ));
            let error = service.set(&tenant, update.clone()).await.unwrap_err();
            if conflict {
                assert!(matches!(error, ComputeError::InvalidRequest(ref message)
                    if message == "payment policy version conflict; retry with current version"));
            } else {
                assert!(matches!(error, ComputeError::Provider(ref message)
                    if message == "payment policy storage unavailable"));
            }
            assert!(patch_error.lock().unwrap().is_none());
            assert_eq!(service.get(&tenant, &hostname).await.unwrap(), first);
        }
        assert_eq!(
            service.set(&tenant, update.clone()).await.unwrap().version,
            2
        );
        let deletion = DeletePaymentPolicyRequest {
            hostname: hostname.clone(),
            expected_version: 2,
        };
        assert!(service.delete(&other, deletion.clone()).await.is_err());
        let deleted = service
            .delete(&tenant, deletion.clone())
            .await
            .unwrap()
            .unwrap();
        assert!(deleted.deleted);
        assert_eq!(deleted.version, 3);
        assert_eq!(
            service.delete(&tenant, deletion).await.unwrap().unwrap(),
            deleted
        );
        assert!(service.resolve(&hostname, "/summary").await.is_err());
        update.expected_version = 3;
        let restored = service.set(&tenant, update).await.unwrap();
        assert_eq!(restored.version, 4);
        assert!(!restored.deleted);
        task.abort();
        let _ = task.await;
    }
}
