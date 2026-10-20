use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;

use crate::driver::{DataDriver, DataError, Result, channel_lease_key};
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

    fn physical_id_from_name(&self, tenant: &Tenant, name: &str) -> String {
        format!("{GATEWAY_PREFIX}{}-{name}", tenant.key)
    }

    fn store_resource_name(&self, tenant: &Tenant, id: &str) -> Result<String> {
        Ok(format!(
            "projects/{}/databases/{}/documents/pay-data/{}/documentStores/{}",
            self.config.project,
            self.config.database,
            tenant.key,
            self.physical_id(tenant, id)?
        ))
    }

    fn document_resource_name(&self, tenant: &Tenant, store_id: &str, key: &str) -> Result<String> {
        validate_key(key)?;
        Ok(format!(
            "{}/documents/{key}",
            self.store_resource_name(tenant, store_id)?
        ))
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

    async fn require_store(&self, tenant: &Tenant, id: &str) -> Result<Value> {
        self.require_json(Method::GET, self.store_url(tenant, id)?, None)
            .await
    }

    async fn require_owned_store(&self, tenant: &Tenant, id: &str) -> Result<Value> {
        let store = self.require_store(tenant, id).await?;
        require_store_access(tenant, &store)?;
        Ok(store)
    }

    async fn require_ready_store(&self, tenant: &Tenant, id: &str) -> Result<Value> {
        let store = self.require_owned_store(tenant, id).await?;
        require_ready(&store)?;
        Ok(store)
    }

    async fn commit_document_write(
        &self,
        _tenant: &Tenant,
        store: &Value,
        write: Value,
    ) -> Result<Value> {
        let store_name = store
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| DataError::Provider("store omitted resource name".into()))?;
        let update_time = store
            .get("updateTime")
            .and_then(Value::as_str)
            .ok_or_else(|| DataError::Provider("store omitted updateTime".into()))?;
        self.require_json(
            Method::POST,
            format!(
                "{}/v1/projects/{}/databases/{}/documents:commit",
                self.config.api_base, self.config.project, self.config.database
            ),
            Some(&json!({
                "writes": [
                    {
                        "verify": store_name,
                        "currentDocument": { "updateTime": update_time }
                    },
                    write
                ]
            })),
        )
        .await
    }

    /// Read `key` from the exact `store` incarnation the caller authorized.
    ///
    /// The store path is reusable: it can be deleted and recreated under
    /// another channel after `store` was read, and a `retain` store leaves
    /// its children behind for the next incarnation. So the parent and child
    /// are read in one `batchGet` snapshot, the parent must still be the
    /// authorized incarnation (same `createTime`, lease and ready phase), and
    /// the child must have been created within it.
    async fn read_document(&self, store: &Value, key: &str) -> Result<Document> {
        validate_key(key)?;
        let store_name = store
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| DataError::Provider("store omitted resource name".into()))?;
        let generation = store
            .get("createTime")
            .and_then(Value::as_str)
            .ok_or_else(|| DataError::Provider("store omitted createTime".into()))?;
        let document_name = format!("{store_name}/documents/{key}");
        let entries = self
            .require_json(
                Method::POST,
                format!("{}:batchGet", self.documents_root()),
                Some(&json!({ "documents": [store_name, document_name] })),
            )
            .await?;
        let found = |name: &str| {
            entries.as_array().into_iter().flatten().find_map(|entry| {
                entry
                    .get("found")
                    .filter(|found| found.get("name").and_then(Value::as_str) == Some(name))
            })
        };
        let current = found(store_name).ok_or_else(store_changed)?;
        if current.get("createTime").and_then(Value::as_str) != Some(generation)
            || firestore_string(current, "payChannel") != firestore_string(store, "payChannel")
            || firestore_string(current, "tenant") != firestore_string(store, "tenant")
        {
            return Err(store_changed());
        }
        require_ready(current)?;
        let document = found(&document_name).ok_or_else(|| {
            DataError::Provider("Firestore returned 404 Not Found: document not found".into())
        })?;
        // Writes tag the incarnation they were made in; an untagged document
        // predates tagging and belongs here only if created after the store.
        let current_incarnation = match firestore_string(document, "storeGeneration") {
            Some(tag) => tag == generation,
            None => {
                let created = document
                    .get("createTime")
                    .and_then(Value::as_str)
                    .and_then(firestore_timestamp)
                    .ok_or_else(|| DataError::Provider("document omitted createTime".into()))?;
                firestore_timestamp(generation).is_some_and(|generation| created >= generation)
            }
        };
        if !current_incarnation {
            return Err(DataError::InvalidRequest(
                "document was retained from an earlier incarnation of this store".into(),
            ));
        }
        normalize_document(store_physical_id(store)?, key, document.clone())
    }

    async fn delete_store_snapshot(&self, tenant: &Tenant, store: &Value) -> Result<()> {
        let physical_id = store_physical_id(store)?;
        let policy = firestore_string(store, "reclaimPolicy").unwrap_or("delete");
        let mut url = url::Url::parse(&self.store_url(tenant, physical_id)?)
            .map_err(|error| DataError::Configuration(error.to_string()))?;
        let update_time = store
            .get("updateTime")
            .and_then(Value::as_str)
            .ok_or_else(|| DataError::Provider("store omitted updateTime".into()))?;
        url.query_pairs_mut()
            .append_pair("currentDocument.updateTime", update_time);
        // Never refresh this snapshot: a concurrent deleter can finish and the
        // owner can recreate the same path while this operation is suspended.
        let deleting = match firestore_string(store, "phase").unwrap_or("ready") {
            "ready" => {
                url.query_pairs_mut()
                    .append_pair("updateMask.fieldPaths", "phase");
                self.require_json(
                    Method::PATCH,
                    url.into(),
                    Some(&json!({ "fields": { "phase": { "stringValue": "deleting" } } })),
                )
                .await?
            }
            "deleting" => store.clone(),
            phase => {
                return Err(DataError::Provider(format!(
                    "document store is in `{phase}` phase"
                )));
            }
        };
        let update_time = deleting
            .get("updateTime")
            .and_then(Value::as_str)
            .ok_or_else(|| DataError::Provider("store omitted updateTime".into()))?;
        if policy == "delete" {
            self.delete_documents(tenant, physical_id, &deleting)
                .await?;
        }
        let mut url = url::Url::parse(&self.store_url(tenant, physical_id)?)
            .map_err(|error| DataError::Configuration(error.to_string()))?;
        url.query_pairs_mut()
            .append_pair("currentDocument.updateTime", update_time);
        self.require_json(Method::DELETE, url.into(), None).await?;
        Ok(())
    }

    async fn delete_documents(
        &self,
        tenant: &Tenant,
        store_id: &str,
        deleting: &Value,
    ) -> Result<()> {
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
                // The parent fence and child delete must be atomic. A separate
                // check would still allow deletion of a replacement's child.
                self.commit_document_write(tenant, deleting, json!({ "delete": name }))
                    .await?;
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
        let physical_id = self.physical_id_from_name(tenant, &request.name);
        let store_url = self.store_url(tenant, &physical_id)?;
        let (status, existing) = self.send_json(Method::GET, store_url.clone(), None).await?;
        let mut write_url = url::Url::parse(&store_url)
            .map_err(|error| DataError::Configuration(error.to_string()))?;
        match status {
            StatusCode::OK => {
                if firestore_string(&existing, "phase") == Some("deleting") {
                    return Err(DataError::InvalidRequest(
                        "document store is being deleted; retry after deletion completes".into(),
                    ));
                }
                require_channel_lease(&tenant.channel_id, &existing)?;
                let update_time = existing
                    .get("updateTime")
                    .and_then(Value::as_str)
                    .ok_or_else(|| DataError::Provider("store omitted updateTime".into()))?;
                write_url
                    .query_pairs_mut()
                    .append_pair("currentDocument.updateTime", update_time);
            }
            StatusCode::NOT_FOUND => {
                write_url
                    .query_pairs_mut()
                    .append_pair("currentDocument.exists", "false");
            }
            _ => return Err(provider_error(status, &existing)),
        }
        let body = json!({
            "fields": {
                "logicalName": { "stringValue": request.name },
                "driver": { "stringValue": DRIVER_ID },
                "class": { "stringValue": CLASS_ID },
                "region": { "stringValue": region },
                "reclaimPolicy": { "stringValue": reclaim_policy_name(&request.reclaim_policy) },
                "gatewayReads": { "booleanValue": request.access.gateway_reads },
                "tenant": { "stringValue": tenant.key },
                "payChannel": { "stringValue": channel_lease_key(&tenant.channel_id)? },
                "phase": { "stringValue": "ready" }
            }
        });
        let value = self
            .require_json(Method::PATCH, write_url.into(), Some(&body))
            .await?;
        normalize_store(value)
    }

    async fn get_document_store(
        &self,
        tenant: &Tenant,
        request: DocumentStoreRequest,
    ) -> Result<DocumentStore> {
        normalize_store(self.require_owned_store(tenant, &request.id).await?)
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
            .filter(|store| require_store_access(tenant, store).is_ok())
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
        let store = self.require_owned_store(tenant, &request.id).await?;
        self.delete_store_snapshot(tenant, &store).await
    }

    async fn get_document(&self, tenant: &Tenant, request: DocumentRequest) -> Result<Document> {
        let store = self.require_ready_store(tenant, &request.store_id).await?;
        self.read_document(&store, &request.key).await
    }

    async fn put_document(&self, tenant: &Tenant, request: PutDocumentRequest) -> Result<Document> {
        let store = self.require_ready_store(tenant, &request.store_id).await?;
        let id = store_physical_id(&store)?;
        let payload = serde_json::to_string(&request.value)?;
        if payload.len() > MAX_DOCUMENT_BYTES {
            return Err(DataError::InvalidRequest(format!(
                "document exceeds {MAX_DOCUMENT_BYTES} bytes after JSON serialization"
            )));
        }
        let document_name = self.document_resource_name(tenant, id, &request.key)?;
        // Tags the write with the incarnation `commit_document_write` verifies.
        let generation = store
            .get("createTime")
            .and_then(Value::as_str)
            .ok_or_else(|| DataError::Provider("store omitted createTime".into()))?;
        let result = self
            .commit_document_write(
                tenant,
                &store,
                json!({
                    "update": {
                        "name": document_name,
                        "fields": {
                            "payload": { "stringValue": payload },
                            "storeGeneration": { "stringValue": generation }
                        }
                    }
                }),
            )
            .await?;
        let updated_at = result
            .pointer("/writeResults/1/updateTime")
            .or_else(|| result.get("commitTime"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(Document {
            store_id: id.to_string(),
            key: request.key,
            value: request.value,
            created_at: None,
            updated_at,
        })
    }

    async fn delete_document(&self, tenant: &Tenant, request: DocumentRequest) -> Result<()> {
        let store = self.require_ready_store(tenant, &request.store_id).await?;
        let id = store_physical_id(&store)?;
        let document_name = self.document_resource_name(tenant, id, &request.key)?;
        self.commit_document_write(tenant, &store, json!({ "delete": document_name }))
            .await?;
        Ok(())
    }

    async fn read_gateway(&self, request: GatewayReadRequest) -> Result<GatewayRead> {
        validate_gateway_store_id(&request.store_id)?;
        let tenant_key = tenant_from_store_id(&request.store_id)?;
        let tenant = Tenant {
            payer: String::new(),
            key: tenant_key.to_string(),
            channel_id: String::new(),
            binding: None,
        };
        // Published reads are public by design; the paid gateway authorizes them.
        let store = self.require_store(&tenant, &request.store_id).await?;
        require_ready(&store)?;
        if firestore_bool(&store, "gatewayReads") != Some(true) {
            return Err(DataError::InvalidRequest(
                "store owner has not published paid gateway reads".into(),
            ));
        }
        let document = self.read_document(&store, &request.key).await?;
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

    async fn cleanup_channel(&self, channel_id: &str) -> Result<usize> {
        let lease_key = channel_lease_key(channel_id)?;
        let url = format!(
            "{}/v1/projects/{}/databases/{}/documents:runQuery",
            self.config.api_base, self.config.project, self.config.database
        );
        let value = self
            .require_json(
                Method::POST,
                url,
                Some(&json!({
                    "structuredQuery": {
                        "from": [{ "collectionId": "documentStores", "allDescendants": true }],
                        "where": {
                            "fieldFilter": {
                                "field": { "fieldPath": "payChannel" },
                                "op": "EQUAL",
                                "value": { "stringValue": lease_key }
                            }
                        }
                    }
                })),
            )
            .await?;
        let stores = value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.get("document"))
            .cloned()
            .collect::<Vec<_>>();
        for store in &stores {
            let tenant_key = firestore_string(store, "tenant")
                .ok_or_else(|| DataError::Provider("store omitted tenant".into()))?;
            require_channel_lease(channel_id, store)?;
            self.delete_store_snapshot(
                &Tenant {
                    payer: String::new(),
                    key: tenant_key.to_string(),
                    channel_id: channel_id.to_string(),
                    binding: None,
                },
                store,
            )
            .await?;
        }
        Ok(stores.len())
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
    let phase = match firestore_string(&value, "phase") {
        Some("deleting") => ResourcePhase::Deleting,
        Some("failed") => ResourcePhase::Failed,
        _ => ResourcePhase::Ready,
    };
    let ready = matches!(phase, ResourcePhase::Ready);
    Ok(DocumentStore {
        driver: DRIVER_ID.into(),
        class: CLASS_ID.into(),
        id: id.clone(),
        name,
        region,
        phase,
        conditions: vec![Condition {
            kind: "Ready".into(),
            status: ready,
            reason: if ready { "Reconciled" } else { "Deleting" }.into(),
            message: if ready {
                "Document store namespace is ready"
            } else {
                "Document store namespace is being deleted"
            }
            .into(),
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

fn store_changed() -> DataError {
    DataError::InvalidRequest("document store changed while it was being read".into())
}

/// A Firestore RFC 3339 UTC timestamp as a comparable `(seconds, nanos)` key:
/// `YYYY-MM-DDTHH:MM:SS[.fraction]Z`, with the fraction padded to nanos.
fn firestore_timestamp(value: &str) -> Option<(String, u32)> {
    let value = value.strip_suffix('Z')?;
    let (seconds, fraction) = value.split_once('.').unwrap_or((value, ""));
    let shape = seconds.len() == 19
        && seconds
            .bytes()
            .enumerate()
            .all(|(index, byte)| match index {
                4 | 7 => byte == b'-',
                10 => byte == b'T',
                13 | 16 => byte == b':',
                _ => byte.is_ascii_digit(),
            });
    if !shape || fraction.len() > 9 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let nanos = format!("{fraction:0<9}").parse().ok()?;
    Some((seconds.to_string(), nanos))
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
    // The physical ID namespace stays reserved so references are unambiguous.
    if name.starts_with(GATEWAY_PREFIX) {
        return Err(DataError::InvalidRequest(format!(
            "document store names may not begin with the reserved `{GATEWAY_PREFIX}` prefix"
        )));
    }
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

fn require_channel_lease(channel_id: &str, value: &Value) -> Result<()> {
    let expected = channel_lease_key(channel_id)?;
    if firestore_string(value, "payChannel") != Some(expected.as_str()) {
        return Err(DataError::InvalidRequest(
            "document store is leased to a different payment channel; use a new name".into(),
        ));
    }
    Ok(())
}

fn require_ready(store: &Value) -> Result<()> {
    if !matches!(firestore_string(store, "phase"), None | Some("ready")) {
        return Err(DataError::InvalidRequest(
            "document store is not ready for operations".into(),
        ));
    }
    Ok(())
}

/// Every payer-scoped request needs the store's funding lease: the verified
/// channel for control calls, or the incarnation and lease a capability pins.
fn require_store_access(tenant: &Tenant, store: &Value) -> Result<()> {
    if firestore_string(store, "tenant") != Some(tenant.key.as_str()) {
        return Err(DataError::InvalidRequest(
            "document store belongs to a different payer".into(),
        ));
    }
    let Some(binding) = &tenant.binding else {
        return require_channel_lease(&tenant.channel_id, store);
    };
    if store.get("createTime").and_then(Value::as_str) != Some(binding.generation.as_str())
        || firestore_string(store, "payChannel") != Some(binding.channel_lease.as_str())
    {
        return Err(DataError::InvalidRequest(
            "data capability was issued for a different store incarnation".into(),
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
mod race_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::response::Response;

    pub(super) fn json_response(status: StatusCode, value: Value) -> Response {
        Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn cleanup_deletes_document_stores_leased_to_the_channel() {
        let lease_key = channel_lease_key("verifiedchannel").unwrap();
        let store_path = "/v1/projects/project/databases/(default)/documents/pay-data/0123456789abcdef/documentStores/fds-0123456789abcdef-weather";
        let document = json!({
            "name": "projects/project/databases/(default)/documents/pay-data/0123456789abcdef/documentStores/fds-0123456789abcdef-weather",
            "updateTime": "2026-01-01T00:00:00Z",
            "fields": {
                "logicalName": { "stringValue": "weather" },
                "driver": { "stringValue": DRIVER_ID },
                "class": { "stringValue": CLASS_ID },
                "region": { "stringValue": "us-central1" },
                "reclaimPolicy": { "stringValue": "delete" },
                "gatewayReads": { "booleanValue": false },
                "tenant": { "stringValue": "0123456789abcdef" },
                "payChannel": { "stringValue": lease_key },
                "phase": { "stringValue": "ready" }
            }
        });
        let app = Router::new().fallback(move |request: Request<Body>| {
            let document = document.clone();
            let lease_key = lease_key.clone();
            async move {
                let method = request.method().clone();
                let path = request.uri().path().to_string();
                match (method.as_str(), path.as_str()) {
                    ("POST", "/v1/projects/project/databases/(default)/documents:runQuery") => {
                        let body = axum::body::to_bytes(request.into_body(), 64 * 1024)
                            .await
                            .unwrap();
                        let body: Value = serde_json::from_slice(&body).unwrap();
                        if body.pointer("/structuredQuery/from/0/collectionId")
                            != Some(&Value::String("documentStores".into()))
                            || body.pointer("/structuredQuery/where/fieldFilter/value/stringValue")
                                != Some(&Value::String(lease_key))
                        {
                            return json_response(StatusCode::BAD_REQUEST, json!({}));
                        }
                        json_response(StatusCode::OK, json!([{ "document": document }]))
                    }
                    ("GET", path) if path == store_path => json_response(StatusCode::OK, document),
                    ("PATCH", path) if path == store_path => {
                        json_response(StatusCode::OK, document)
                    }
                    ("GET", path) if path == format!("{store_path}/documents") => {
                        json_response(StatusCode::OK, json!({}))
                    }
                    ("DELETE", path) if path == store_path => {
                        json_response(StatusCode::OK, json!({}))
                    }
                    _ => json_response(StatusCode::NOT_FOUND, json!({ "path": path })),
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let driver = FirestoreDriver::new(FirestoreConfig {
            project: "project".into(),
            database: "(default)".into(),
            region: "us-central1".into(),
            api_base: base,
            metadata_base: "http://metadata.invalid".into(),
            access_token: Some("token".into()),
        })
        .unwrap();

        assert_eq!(driver.cleanup_channel("verifiedchannel").await.unwrap(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn deletion_resumes_and_stale_writes_use_atomic_preconditions() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let commits = Arc::new(AtomicUsize::new(0));
        let observed = commits.clone();
        let store_name = "projects/project/databases/(default)/documents/pay-data/0123456789abcdef/documentStores/fds-0123456789abcdef-weather";
        let store = json!({
            "name": store_name, "updateTime": "2026-01-01T00:00:00Z",
            "fields": {
                "phase": { "stringValue": "deleting" },
                "reclaimPolicy": { "stringValue": "delete" },
                "tenant": { "stringValue": "0123456789abcdef" },
                "payChannel": { "stringValue": channel_lease_key("channel").unwrap() }
            }
        });
        let deleting = store.clone();
        let app = Router::new().fallback(move |request: Request<Body>| {
            let store = deleting.clone();
            let observed = observed.clone();
            async move {
                match request.method().as_str() {
                    "GET" if request.uri().path().ends_with("/documents") => {
                        json_response(StatusCode::OK, json!({}))
                    }
                    "GET" => json_response(StatusCode::OK, store),
                    "DELETE" => json_response(StatusCode::OK, json!({})),
                    "POST" => {
                        let body = axum::body::to_bytes(request.into_body(), 65536)
                            .await
                            .unwrap();
                        let body: Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(body["writes"][0]["verify"], store_name);
                        assert_eq!(
                            body["writes"][0]["currentDocument"]["updateTime"],
                            "2026-01-01T00:00:00Z"
                        );
                        assert_eq!(body["writes"].as_array().unwrap().len(), 2);
                        observed.fetch_add(1, Ordering::SeqCst);
                        json_response(
                            StatusCode::CONFLICT,
                            json!({ "error": { "message": "stale parent" } }),
                        )
                    }
                    _ => panic!("resuming deletion must not reset the parent phase"),
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let driver = FirestoreDriver::new(FirestoreConfig {
            project: "project".into(),
            database: "(default)".into(),
            region: "us-central1".into(),
            api_base: base,
            metadata_base: "http://metadata.invalid".into(),
            access_token: Some("token".into()),
        })
        .unwrap();
        let tenant = Tenant {
            payer: "payer".into(),
            key: "0123456789abcdef".into(),
            channel_id: "channel".into(),
            binding: None,
        };
        assert!(
            driver
                .require_ready_store(&tenant, "weather")
                .await
                .is_err()
        );
        driver
            .delete_document_store(
                &tenant,
                DocumentStoreRequest {
                    driver: DRIVER_ID.into(),
                    id: "weather".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            driver
                .commit_document_write(
                    &tenant,
                    &store,
                    json!({ "delete": format!("{store_name}/documents/key") })
                )
                .await
                .is_err()
        );
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[test]
    fn firestore_timestamps_order_across_fraction_precision() {
        let at = |value| firestore_timestamp(value).unwrap();
        assert!(at("2026-01-01T00:00:00.5Z") > at("2026-01-01T00:00:00.123456Z"));
        assert_eq!(at("2026-01-01T00:00:00Z"), at("2026-01-01T00:00:00.000Z"));
        assert!(at("2026-01-01T00:00:01Z") > at("2026-01-01T00:00:00.999999999Z"));
        for invalid in [
            "",
            "2026-01-01T00:00:00",
            "2026-01-01 00:00:00Z",
            "created-1",
        ] {
            assert!(firestore_timestamp(invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn channel_ids_map_to_stable_lease_keys() {
        assert_eq!(
            channel_lease_key("verifiedchannel").unwrap(),
            "8e8a355d709e16245dcd6748262bec1a"
        );
        assert!(channel_lease_key("").is_err());
        assert!(
            require_channel_lease(
                "verifiedchannel",
                &json!({ "fields": { "payChannel": { "stringValue": "other" } } })
            )
            .is_err()
        );
    }

    #[test]
    fn store_ids_are_tenant_scoped() {
        let tenant = Tenant {
            payer: "payer".into(),
            key: "0123456789abcdef".into(),
            channel_id: "channel".into(),
            binding: None,
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
        assert!(validate_name("fds-weather").is_err());
        assert!(validate_name("fds-0123456789abcdef-weather").is_err());
        assert!(validate_name("fdsweather").is_ok());
        assert!(validate_name("weather-fds-").is_err());
        assert!(validate_name("weather-fds-x").is_ok());
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
