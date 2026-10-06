use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::types::{
    CreateDocumentStoreRequest, DataClass, Document, DocumentRequest, DocumentStore,
    DocumentStoreList, DocumentStoreRequest, GatewayRead, GatewayReadRequest,
    ListDocumentStoresRequest, PutDocumentRequest, Tenant,
};

#[derive(Debug, Error)]
pub enum DataError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("driver `{0}` is not configured")]
    DriverNotFound(String),
    #[error("configuration error: {0}")]
    Configuration(String),
    #[error("provider request failed: {0}")]
    Provider(String),
    #[error("transport failed")]
    Transport(#[source] reqwest::Error),
    #[error("response serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl From<reqwest::Error> for DataError {
    fn from(error: reqwest::Error) -> Self {
        Self::Transport(error)
    }
}

pub type Result<T> = std::result::Result<T, DataError>;

#[async_trait]
pub trait DataDriver: Send + Sync {
    fn id(&self) -> &'static str;
    fn gateway_prefix(&self) -> &'static str;
    fn classes(&self) -> Vec<DataClass>;
    async fn create_document_store(
        &self,
        tenant: &Tenant,
        request: CreateDocumentStoreRequest,
    ) -> Result<DocumentStore>;
    async fn get_document_store(
        &self,
        tenant: &Tenant,
        request: DocumentStoreRequest,
    ) -> Result<DocumentStore>;
    async fn list_document_stores(
        &self,
        tenant: &Tenant,
        request: ListDocumentStoresRequest,
    ) -> Result<DocumentStoreList>;
    async fn delete_document_store(
        &self,
        tenant: &Tenant,
        request: DocumentStoreRequest,
    ) -> Result<()>;
    async fn get_document(&self, tenant: &Tenant, request: DocumentRequest) -> Result<Document>;
    async fn put_document(&self, tenant: &Tenant, request: PutDocumentRequest) -> Result<Document>;
    async fn delete_document(&self, tenant: &Tenant, request: DocumentRequest) -> Result<()>;
    async fn read_gateway(&self, request: GatewayReadRequest) -> Result<GatewayRead>;
}

#[derive(Clone, Default)]
pub struct DriverRegistry {
    drivers: Arc<BTreeMap<String, Arc<dyn DataDriver>>>,
}

impl DriverRegistry {
    pub fn new(drivers: impl IntoIterator<Item = Arc<dyn DataDriver>>) -> Result<Self> {
        let mut by_id = BTreeMap::new();
        for driver in drivers {
            let id = driver.id().to_string();
            if by_id.insert(id.clone(), driver).is_some() {
                return Err(DataError::Configuration(format!(
                    "duplicate data driver `{id}`"
                )));
            }
        }
        Ok(Self {
            drivers: Arc::new(by_id),
        })
    }

    pub fn classes(&self) -> Vec<DataClass> {
        self.drivers
            .values()
            .flat_map(|driver| driver.classes())
            .collect()
    }

    pub fn get(&self, id: &str) -> Result<Arc<dyn DataDriver>> {
        self.drivers
            .get(id)
            .cloned()
            .ok_or_else(|| DataError::DriverNotFound(id.to_string()))
    }

    pub fn for_gateway(&self, id: &str) -> Result<Arc<dyn DataDriver>> {
        self.drivers
            .values()
            .find(|driver| id.starts_with(driver.gateway_prefix()))
            .cloned()
            .ok_or_else(|| DataError::DriverNotFound(id.to_string()))
    }
}
