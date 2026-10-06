use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::types::{CreateWalletRequest, DriverCapabilities, Tenant, Wallet, WalletRequest};

#[derive(Debug, Error)]
pub enum WalletError {
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
}
pub type Result<T> = std::result::Result<T, WalletError>;

#[async_trait]
pub trait WalletDriver: Send + Sync {
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> DriverCapabilities;
    async fn create(&self, tenant: &Tenant, request: CreateWalletRequest) -> Result<Wallet>;
    async fn get(&self, tenant: &Tenant, request: WalletRequest) -> Result<Wallet>;
}

#[derive(Clone, Default)]
pub struct DriverRegistry {
    drivers: Arc<BTreeMap<String, Arc<dyn WalletDriver>>>,
}
impl DriverRegistry {
    pub fn new(drivers: impl IntoIterator<Item = Arc<dyn WalletDriver>>) -> Result<Self> {
        let mut by_id = BTreeMap::new();
        for driver in drivers {
            let id = driver.id().to_string();
            if by_id.insert(id.clone(), driver).is_some() {
                return Err(WalletError::Configuration(format!(
                    "duplicate wallet driver `{id}`"
                )));
            }
        }
        Ok(Self {
            drivers: Arc::new(by_id),
        })
    }
    pub fn capabilities(&self) -> Vec<DriverCapabilities> {
        self.drivers.values().map(|d| d.capabilities()).collect()
    }
    pub fn get(&self, id: &str) -> Result<Arc<dyn WalletDriver>> {
        self.drivers
            .get(id)
            .cloned()
            .ok_or_else(|| WalletError::DriverNotFound(id.into()))
    }
}
