use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::types::{
    CreateJobRequest, DriverCapabilities, ExecuteJobRequest, Job, JobList, JobRequest,
    ListJobsRequest, Tenant,
};

#[derive(Debug, Error)]
pub enum JobError {
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

pub type Result<T> = std::result::Result<T, JobError>;

pub struct ExecutionResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: bytes::Bytes,
}

#[async_trait]
pub trait JobDriver: Send + Sync {
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> DriverCapabilities;
    async fn create(&self, tenant: &Tenant, request: CreateJobRequest) -> Result<Job>;
    async fn get(&self, tenant: &Tenant, request: JobRequest) -> Result<Job>;
    async fn list(&self, tenant: &Tenant, request: ListJobsRequest) -> Result<JobList>;
    async fn pause(&self, tenant: &Tenant, request: JobRequest) -> Result<Job>;
    async fn resume(&self, tenant: &Tenant, request: JobRequest) -> Result<Job>;
    async fn run_now(&self, tenant: &Tenant, request: JobRequest) -> Result<Job>;
    async fn delete(&self, tenant: &Tenant, request: JobRequest) -> Result<()>;
    async fn cleanup_channel(&self, channel_id: &str) -> Result<usize>;
    async fn execute(&self, request: ExecuteJobRequest) -> Result<ExecutionResponse>;
}

#[derive(Clone, Default)]
pub struct DriverRegistry {
    drivers: Arc<BTreeMap<String, Arc<dyn JobDriver>>>,
}

impl DriverRegistry {
    pub fn new(drivers: impl IntoIterator<Item = Arc<dyn JobDriver>>) -> Result<Self> {
        let mut by_id = BTreeMap::new();
        for driver in drivers {
            let id = driver.id().to_string();
            if by_id.insert(id.clone(), driver).is_some() {
                return Err(JobError::Configuration(format!(
                    "duplicate jobs driver `{id}`"
                )));
            }
        }
        Ok(Self {
            drivers: Arc::new(by_id),
        })
    }

    pub fn capabilities(&self) -> Vec<DriverCapabilities> {
        self.drivers
            .values()
            .map(|driver| driver.capabilities())
            .collect()
    }

    pub fn get(&self, id: &str) -> Result<Arc<dyn JobDriver>> {
        self.drivers
            .get(id)
            .cloned()
            .ok_or_else(|| JobError::DriverNotFound(id.into()))
    }
}
