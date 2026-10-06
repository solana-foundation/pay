use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;

use crate::driver::{DataDriver, DataError, Result};
use crate::types::{
    Condition, CreateDocumentStoreRequest, DataClass, Document, DocumentRequest, DocumentStore,
    DocumentStoreList, DocumentStoreRequest, GatewayRead, GatewayReadRequest,
    ListDocumentStoresRequest, PutDocumentRequest, ReclaimPolicy, ResourcePhase, Tenant,
};

pub const DRIVER_ID: &str = "gcp-firestore";
pub const CLASS_ID: &str = "document/serverless";
pub const GATEWAY_PREFIX: &str = "fds-";
const DEFAULT_API_BASE: &str = "https://firestore.googleapis.com";
const DEFAULT_METADATA_BASE: &str = "http://metadata.google.internal/computeMetadata/v1";
const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
const MAX_DELETE_DOCUMENTS: usize = 10_000;
const READ_MICRO_USD: f64 = 0.30;
const GATEWAY_READ_OPERATIONS: f64 = 2.0;
const EGRESS_GIB_USD: f64 = 0.12;
const PLATFORM_MULTIPLIER: f64 = 1.20;

#[derive(Clone)]
pub struct FirestoreConfig {
    pub project: String,
    pub database: String,
    pub region: String,
    pub api_base: String,
    pub metadata_base: String,
    pub access_token: Option<String>,
}

impl FirestoreConfig {
    pub fn from_env() -> Result<Self> {
        let project = env_nonempty("DATA_GCP_PROJECT")
            .or_else(|| env_nonempty("GOOGLE_CLOUD_PROJECT"))
            .ok_or_else(|| {
                DataError::Configuration(
                    "DATA_GCP_PROJECT or GOOGLE_CLOUD_PROJECT must be set".into(),
                )
            })?;
        Ok(Self {
            project,
            database: env_nonempty("DATA_FIRESTORE_DATABASE").unwrap_or_else(|| "(default)".into()),
            region: env_nonempty("DATA_GCP_REGION").unwrap_or_else(|| "us-central1".into()),
            api_base: env_nonempty("DATA_FIRESTORE_API_BASE")
                .unwrap_or_else(|| DEFAULT_API_BASE.into())
                .trim_end_matches('/')
                .to_string(),
            metadata_base: env_nonempty("DATA_GCP_METADATA_BASE")
                .unwrap_or_else(|| DEFAULT_METADATA_BASE.into())
                .trim_end_matches('/')
                .to_string(),
            access_token: env_nonempty("DATA_GCP_ACCESS_TOKEN")
                .or_else(|| env_nonempty("GOOGLE_OAUTH_ACCESS_TOKEN")),
        })
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[derive(Clone)]
pub struct FirestoreDriver {
    config: Arc<FirestoreConfig>,
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

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct FirestoreOptions {}

impl FirestoreDriver {
    pub fn new(config: FirestoreConfig) -> Result<Self> {
        validate_segment("project", &config.project, true)?;
        validate_segment("database", &config.database, true)?;
        validate_segment("region", &config.region, false)?;
        Ok(Self {
            config: Arc::new(config),
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()?,
            token: Arc::new(RwLock::new(None)),
        })
    }

    async fn access_token(&self) -> Result<String> {
        if let Some(token) = &self.config.access_token {
            return Ok(token.clone());
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
                self.config.metadata_base
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

    fn documents_root(&self) -> String {
        format!(
            "{}/v1/projects/{}/databases/{}/documents",
            self.config.api_base, self.config.project, self.config.database
        )
    }

    fn physical_id(&self, tenant: &Tenant, id: &str) -> Result<String> {
        if id.starts_with(GATEWAY_PREFIX) {
            validate_owned_store_id(tenant, id)?;
            Ok(id.to_string())
        } else {
            validate_name(id)?;
            Ok(format!("{GATEWAY_PREFIX}{}-{id}", tenant.key))
        }
    }

    fn store_url(&self, tenant: &Tenant, id: &str) -> Result<String> {
        Ok(format!(
            "{}/pay-data/{}/documentStores/{}",
            self.documents_root(),
            tenant.key,
            self.physical_id(tenant, id)?
        ))
    }

    fn store_collection_url(&self, tenant: &Tenant) -> String {
        format!(
            "{}/pay-data/{}/documentStores",
            self.documents_root(),
            tenant.key
        )
    }

    fn document_url(&self, tenant: &Tenant, store_id: &str, key: &str) -> Result<String> {
        validate_key(key)?;
        Ok(format!(
            "{}/documents/{key}",
            self.store_url(tenant, store_id)?
        ))
    }

    async fn require_store(&self, tenant: &Tenant, id: &str) -> Result<Value> {
        self.require_json(Method::GET, self.store_url(tenant, id)?, None)
            .await
    }

    async fn get_document_from_store(
        &self,
        tenant: &Tenant,
        store_id: &str,
        key: &str,
    ) -> Result<Document> {
        let value = self
            .require_json(Method::GET, self.document_url(tenant, store_id, key)?, None)
            .await?;
        normalize_document(store_id, key, value)
    }

    async fn delete_documents(&self, tenant: &Tenant, store_id: &str) -> Result<()> {
        let mut page_token: Option<String> = None;
        let mut deleted = 0;
        loop {
            let mut url =
                url::Url::parse(&format!("{}/documents", self.store_url(tenant, store_id)?))
                    .map_err(|error| DataError::Configuration(error.to_string()))?;
            url.query_pairs_mut().append_pair("pageSize", "1000");
            if let Some(token) = &page_token {
                url.query_pairs_mut().append_pair("pageToken", token);
            }
            let value = self.require_json(Method::GET, url.into(), None).await?;
            for document in value
                .get("documents")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let name = document
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| DataError::Provider("Firestore document omitted name".into()))?;
                let (status, response) = self
                    .send_json(
                        Method::DELETE,
                        format!("{}/v1/{name}", self.config.api_base),
                        None,
                    )
                    .await?;
                if !status.is_success() {
                    return Err(provider_error(status, &response));
                }
                deleted += 1;
                if deleted > MAX_DELETE_DOCUMENTS {
                    return Err(DataError::InvalidRequest(format!(
                        "store contains more than {MAX_DELETE_DOCUMENTS} documents; bulk deletion requires a background garbage-collection job"
                    )));
                }
            }
            page_token = value
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(str::to_string);
            if page_token.is_none() {
                return Ok(());
            }
        }
    }
}

#[async_trait]
impl DataDriver for FirestoreDriver {
    fn id(&self) -> &'static str {
        DRIVER_ID
    }

    fn gateway_prefix(&self) -> &'static str {
        GATEWAY_PREFIX
    }

    fn classes(&self) -> Vec<DataClass> {
        vec![DataClass {
            driver: DRIVER_ID.into(),
            class: CLASS_ID.into(),
            display_name: "Serverless document store (Google Firestore)".into(),
            data_model: "document".into(),
            operations: vec!["get".into(), "put".into(), "delete".into()],
            default_region: self.config.region.clone(),
            notes: vec![
                "Claims are payer-scoped logical namespaces in Firestore Standard edition.".into(),
                "Only explicitly published reads are available through the paid data gateway."
                    .into(),
            ],
        }]
    }

    async fn create_document_store(
        &self,
        tenant: &Tenant,
        request: CreateDocumentStoreRequest,
    ) -> Result<DocumentStore> {
        validate_name(&request.name)?;
        if request.class != CLASS_ID {
            return Err(DataError::InvalidRequest(format!(
                "unsupported class `{}`; driver `{DRIVER_ID}` supports only `{CLASS_ID}`",
                request.class
            )));
        }
        let region = request.region.as_deref().unwrap_or(&self.config.region);
        if region != self.config.region {
            return Err(DataError::InvalidRequest(format!(
                "Firestore database is placed in `{}`; requested region was `{region}`",
                self.config.region
            )));
        }
        let _: FirestoreOptions = if request.driver_options.is_null() {
            FirestoreOptions::default()
        } else {
            serde_json::from_value(request.driver_options).map_err(|error| {
                DataError::InvalidRequest(format!("invalid Firestore driver_options: {error}"))
            })?
        };
        let physical_id = self.physical_id(tenant, &request.name)?;
        let body = json!({
            "fields": {
                "logicalName": { "stringValue": request.name },
                "driver": { "stringValue": DRIVER_ID },
                "class": { "stringValue": CLASS_ID },
                "region": { "stringValue": region },
                "reclaimPolicy": { "stringValue": reclaim_policy_name(&request.reclaim_policy) },
                "gatewayReads": { "booleanValue": request.access.gateway_reads },
                "tenant": { "stringValue": tenant.key }
            }
        });
        let value = self
            .require_json(
                Method::PATCH,
                self.store_url(tenant, &physical_id)?,
                Some(&body),
            )
            .await?;
        normalize_store(value)
    }

    async fn get_document_store(
        &self,
        tenant: &Tenant,
        request: DocumentStoreRequest,
    ) -> Result<DocumentStore> {
        normalize_store(self.require_store(tenant, &request.id).await?)
    }

    async fn list_document_stores(
        &self,
        tenant: &Tenant,
        request: ListDocumentStoresRequest,
    ) -> Result<DocumentStoreList> {
        if request.page_size == 0 || request.page_size > 1000 {
            return Err(DataError::InvalidRequest(
                "page_size must be between 1 and 1000".into(),
            ));
        }
        let mut url = url::Url::parse(&self.store_collection_url(tenant))
            .map_err(|error| DataError::Configuration(error.to_string()))?;
        url.query_pairs_mut()
            .append_pair("pageSize", &request.page_size.to_string());
        if let Some(token) = request.page_token {
            url.query_pairs_mut().append_pair("pageToken", &token);
        }
        let value = self.require_json(Method::GET, url.into(), None).await?;
        let stores = value
            .get("documents")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .cloned()
            .map(normalize_store)
            .collect::<Result<Vec<_>>>()?;
        Ok(DocumentStoreList {
            stores,
            next_page_token: value
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    async fn delete_document_store(
        &self,
        tenant: &Tenant,
        request: DocumentStoreRequest,
    ) -> Result<()> {
        let store = self.require_store(tenant, &request.id).await?;
        let physical_id = store_physical_id(&store)?;
        let policy = firestore_string(&store, "reclaimPolicy").unwrap_or("delete");
        if policy == "delete" {
            self.delete_documents(tenant, physical_id).await?;
        }
        let (status, response) = self
            .send_json(Method::DELETE, self.store_url(tenant, physical_id)?, None)
            .await?;
        if !status.is_success() {
            return Err(provider_error(status, &response));
        }
        Ok(())
    }

    async fn get_document(&self, tenant: &Tenant, request: DocumentRequest) -> Result<Document> {
        let store = self.require_store(tenant, &request.store_id).await?;
        let id = store_physical_id(&store)?;
        self.get_document_from_store(tenant, id, &request.key).await
    }

    async fn put_document(&self, tenant: &Tenant, request: PutDocumentRequest) -> Result<Document> {
        let store = self.require_store(tenant, &request.store_id).await?;
        let id = store_physical_id(&store)?;
        let payload = serde_json::to_string(&request.value)?;
        if payload.len() > MAX_DOCUMENT_BYTES {
            return Err(DataError::InvalidRequest(format!(
                "document exceeds {MAX_DOCUMENT_BYTES} bytes after JSON serialization"
            )));
        }
        let value = self
            .require_json(
                Method::PATCH,
                self.document_url(tenant, id, &request.key)?,
                Some(&json!({
                    "fields": { "payload": { "stringValue": payload } }
                })),
            )
            .await?;
        normalize_document(id, &request.key, value)
    }

    async fn delete_document(&self, tenant: &Tenant, request: DocumentRequest) -> Result<()> {
        let store = self.require_store(tenant, &request.store_id).await?;
        let id = store_physical_id(&store)?;
        let (status, response) = self
            .send_json(
                Method::DELETE,
                self.document_url(tenant, id, &request.key)?,
                None,
            )
            .await?;
        if !status.is_success() {
            return Err(provider_error(status, &response));
        }
        Ok(())
    }

    async fn read_gateway(&self, request: GatewayReadRequest) -> Result<GatewayRead> {
        validate_gateway_store_id(&request.store_id)?;
        let tenant_key = tenant_from_store_id(&request.store_id)?;
        let tenant = Tenant {
            payer: String::new(),
            key: tenant_key.to_string(),
        };
        let store = self.require_store(&tenant, &request.store_id).await?;
        if firestore_bool(&store, "gatewayReads") != Some(true) {
            return Err(DataError::InvalidRequest(
                "store owner has not published paid gateway reads".into(),
            ));
        }
        let document = self
            .get_document_from_store(&tenant, &request.store_id, &request.key)
            .await?;
        let bytes = serde_json::to_vec(&document.value)?.len();
        let egress_gib = bytes as f64 / 1024_f64.powi(3);
        let billed_micro_usd = ((READ_MICRO_USD * GATEWAY_READ_OPERATIONS
            + egress_gib * EGRESS_GIB_USD * 1_000_000.0)
            * PLATFORM_MULTIPLIER)
            .ceil()
            .max(1.0) as u64;
        Ok(GatewayRead {
            document,
            billed_micro_usd,
        })
    }
}

fn normalize_store(value: Value) -> Result<DocumentStore> {
    let id = store_physical_id(&value)?.to_string();
    let name = firestore_string(&value, "logicalName")
        .ok_or_else(|| DataError::Provider("store omitted logicalName".into()))?
        .to_string();
    let region = firestore_string(&value, "region")
        .ok_or_else(|| DataError::Provider("store omitted region".into()))?
        .to_string();
    let reclaim_policy = match firestore_string(&value, "reclaimPolicy") {
        Some("retain") => ReclaimPolicy::Retain,
        _ => ReclaimPolicy::Delete,
    };
    Ok(DocumentStore {
        driver: DRIVER_ID.into(),
        class: CLASS_ID.into(),
        id: id.clone(),
        name,
        region,
        phase: ResourcePhase::Ready,
        conditions: vec![Condition {
            kind: "Ready".into(),
            status: true,
            reason: "Reconciled".into(),
            message: "Document store namespace is ready".into(),
        }],
        reclaim_policy,
        gateway_id: (firestore_bool(&value, "gatewayReads") == Some(true)).then_some(id),
        created_at: value
            .get("createTime")
            .and_then(Value::as_str)
            .map(str::to_string),
        updated_at: value
            .get("updateTime")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn normalize_document(store_id: &str, key: &str, value: Value) -> Result<Document> {
    let payload = firestore_string(&value, "payload")
        .ok_or_else(|| DataError::Provider("document omitted payload".into()))?;
    Ok(Document {
        store_id: store_id.to_string(),
        key: key.to_string(),
        value: serde_json::from_str(payload)?,
        created_at: value
            .get("createTime")
            .and_then(Value::as_str)
            .map(str::to_string),
        updated_at: value
            .get("updateTime")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn store_physical_id(value: &Value) -> Result<&str> {
    value
        .get("name")
        .and_then(Value::as_str)
        .and_then(|name| name.rsplit('/').next())
        .ok_or_else(|| DataError::Provider("store omitted resource name".into()))
}

fn firestore_string<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value
        .pointer(&format!("/fields/{field}/stringValue"))
        .and_then(Value::as_str)
}

fn firestore_bool(value: &Value, field: &str) -> Option<bool> {
    value
        .pointer(&format!("/fields/{field}/booleanValue"))
        .and_then(Value::as_bool)
}

fn reclaim_policy_name(policy: &ReclaimPolicy) -> &'static str {
    match policy {
        ReclaimPolicy::Delete => "delete",
        ReclaimPolicy::Retain => "retain",
    }
}

fn validate_name(name: &str) -> Result<()> {
    if !(3..=42).contains(&name.len())
        || !name.as_bytes()[0].is_ascii_lowercase()
        || !name.as_bytes()[name.len() - 1].is_ascii_alphanumeric()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(DataError::InvalidRequest(
            "document store name must be 3-42 lowercase letters, digits, or hyphens; start with a letter and end with a letter or digit".into(),
        ));
    }
    Ok(())
}

fn validate_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.len() > 128
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(DataError::InvalidRequest(
            "document key must be 1-128 ASCII letters, digits, `_`, or `-`".into(),
        ));
    }
    Ok(())
}

fn validate_segment(label: &str, value: &str, allow_parentheses: bool) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_')
                || (allow_parentheses && matches!(byte, b'(' | b')' | b'.'))
        })
    {
        return Err(DataError::InvalidRequest(format!("invalid {label}")));
    }
    Ok(())
}

fn validate_gateway_store_id(id: &str) -> Result<()> {
    if !id.starts_with(GATEWAY_PREFIX)
        || id.len() > 63
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(DataError::InvalidRequest(
            "invalid document store gateway ID".into(),
        ));
    }
    tenant_from_store_id(id)?;
    Ok(())
}

fn tenant_from_store_id(id: &str) -> Result<&str> {
    id.strip_prefix(GATEWAY_PREFIX)
        .and_then(|suffix| suffix.split_once('-'))
        .map(|(tenant, _)| tenant)
        .filter(|tenant| tenant.len() == 16 && tenant.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| DataError::InvalidRequest("store ID omitted tenant namespace".into()))
}

fn validate_owned_store_id(tenant: &Tenant, id: &str) -> Result<()> {
    validate_gateway_store_id(id)?;
    if tenant_from_store_id(id)? != tenant.key {
        return Err(DataError::InvalidRequest(
            "document store belongs to a different payer".into(),
        ));
    }
    Ok(())
}

fn provider_error(status: StatusCode, value: &Value) -> DataError {
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("unknown provider error");
    DataError::Provider(format!("Firestore returned {status}: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_ids_are_tenant_scoped() {
        let tenant = Tenant {
            payer: "payer".into(),
            key: "0123456789abcdef".into(),
        };
        let config = FirestoreConfig {
            project: "project".into(),
            database: "(default)".into(),
            region: "us-central1".into(),
            api_base: "https://example.invalid".into(),
            metadata_base: "https://metadata.invalid".into(),
            access_token: Some("test".into()),
        };
        let driver = FirestoreDriver::new(config).unwrap();
        assert_eq!(
            driver.physical_id(&tenant, "weather").unwrap(),
            "fds-0123456789abcdef-weather"
        );
        assert!(
            driver
                .physical_id(&tenant, "fds-fedcba9876543210-weather")
                .is_err()
        );
    }

    #[test]
    fn keys_reject_path_traversal() {
        assert!(validate_key("latest").is_ok());
        assert!(validate_key("../other").is_err());
        assert!(validate_key("a/b").is_err());
    }
}
