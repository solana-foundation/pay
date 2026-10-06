use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::trigger_types::{
    CreateTriggerRequest, ExecuteTriggerRequest, ListTriggersRequest, Trigger,
    TriggerDriverCapabilities, TriggerList, TriggerRequest,
};
use crate::types::Tenant;

#[derive(Debug, Error)]
pub enum TriggerError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("driver `{0}` is not configured")]
    DriverNotFound(String),
    #[error("configuration error: {0}")]
    Configuration(String),
    #[error("provider request failed: {0}")]
    Provider(String),
    #[error("transport failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("response serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("execution budget store failed: {0}")]
    BudgetStore(#[from] redis::RedisError),
}

pub type Result<T> = std::result::Result<T, TriggerError>;

pub struct TriggerExecutionResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: bytes::Bytes,
}

#[async_trait]
pub trait TriggerDriver: Send + Sync {
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> TriggerDriverCapabilities;
    async fn create(&self, tenant: &Tenant, request: CreateTriggerRequest) -> Result<Trigger>;
    async fn get(&self, tenant: &Tenant, request: TriggerRequest) -> Result<Trigger>;
    async fn list(&self, tenant: &Tenant, request: ListTriggersRequest) -> Result<TriggerList>;
    async fn pause(&self, tenant: &Tenant, request: TriggerRequest) -> Result<Trigger>;
    async fn resume(&self, tenant: &Tenant, request: TriggerRequest) -> Result<Trigger>;
    async fn run_now(&self, tenant: &Tenant, request: TriggerRequest) -> Result<Trigger>;
    async fn delete(&self, tenant: &Tenant, request: TriggerRequest) -> Result<()>;
    /// Delete triggers attached to one compute workload. This is called before
    /// the parent workload is deleted so provider schedules cannot become
    /// orphaned invocations.
    async fn cleanup_target(
        &self,
        tenant: &Tenant,
        provider: &str,
        id: &str,
        region: Option<&str>,
    ) -> Result<usize>;
    async fn cleanup_channel(&self, channel_id: &str) -> Result<usize>;
    async fn execute(&self, request: ExecuteTriggerRequest) -> Result<TriggerExecutionResponse>;
}

#[derive(Clone, Default)]
pub struct TriggerDriverRegistry {
    drivers: Arc<BTreeMap<String, Arc<dyn TriggerDriver>>>,
}

impl TriggerDriverRegistry {
    pub fn new(drivers: impl IntoIterator<Item = Arc<dyn TriggerDriver>>) -> Result<Self> {
        let mut by_id = BTreeMap::new();
        for driver in drivers {
            let id = driver.id().to_string();
            if by_id.insert(id.clone(), driver).is_some() {
                return Err(TriggerError::Configuration(format!(
                    "duplicate trigger driver `{id}`"
                )));
            }
        }
        Ok(Self {
            drivers: Arc::new(by_id),
        })
    }

    pub fn capabilities(&self) -> Vec<TriggerDriverCapabilities> {
        self.drivers
            .values()
            .map(|driver| driver.capabilities())
            .collect()
    }

    pub fn get(&self, id: &str) -> Result<Arc<dyn TriggerDriver>> {
        self.drivers
            .get(id)
            .cloned()
            .ok_or_else(|| TriggerError::DriverNotFound(id.into()))
    }

    pub async fn cleanup_target(
        &self,
        tenant: &Tenant,
        provider: &str,
        id: &str,
        region: Option<&str>,
    ) -> Result<usize> {
        let mut deleted = 0;
        for driver in self.drivers.values() {
            deleted += driver.cleanup_target(tenant, provider, id, region).await?;
        }
        Ok(deleted)
    }
}
