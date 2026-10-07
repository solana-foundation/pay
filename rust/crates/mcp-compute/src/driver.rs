use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::types::{
    ComputeOperation, DeployRequest, Deployment, DeploymentList, DriverCapabilities,
    GatewayInvocation, GatewayInvokeRequest, InvocationResult, InvokeRequest, ListRequest,
    ResourceRequest, Tenant,
};

#[derive(Debug, Error)]
pub enum ComputeError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("provider `{0}` is not configured")]
    ProviderNotFound(String),
    #[error("compute resource was not found")]
    ResourceNotFound,
    #[error("configuration error: {0}")]
    Configuration(String),
    #[error("provider request failed: {0}")]
    Provider(String),
    #[error("transport failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("response serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, ComputeError>;

#[async_trait]
pub trait ComputeDriver: Send + Sync {
    fn id(&self) -> &'static str;
    fn gateway_prefix(&self) -> &'static str;
    fn capabilities(&self) -> DriverCapabilities;
    async fn deploy(&self, tenant: &Tenant, request: DeployRequest) -> Result<ComputeOperation>;
    async fn get(&self, tenant: &Tenant, request: ResourceRequest) -> Result<Deployment>;
    async fn list(&self, tenant: &Tenant, request: ListRequest) -> Result<DeploymentList>;
    async fn delete(&self, tenant: &Tenant, request: ResourceRequest) -> Result<ComputeOperation>;
    async fn operation(&self, tenant: &Tenant, id: &str) -> Result<ComputeOperation>;
    async fn invoke(&self, tenant: &Tenant, request: InvokeRequest) -> Result<InvocationResult>;
    async fn invoke_gateway(&self, request: GatewayInvokeRequest) -> Result<GatewayInvocation>;
    /// Delete all provider resources funded by `channel_id`. This must be
    /// idempotent because the lifecycle worker retries until it succeeds.
    async fn cleanup_channel(&self, channel_id: &str) -> Result<usize>;
    /// Reconcile bounded provider-owned metadata independently of channel GC.
    /// Dry runs validate and report candidates without any writes.
    async fn reconcile_orphans(&self, _dry_run: bool) -> Result<usize> {
        Ok(0)
    }
}

#[derive(Clone, Default)]
pub struct DriverRegistry {
    drivers: Arc<BTreeMap<String, Arc<dyn ComputeDriver>>>,
}

impl DriverRegistry {
    pub fn new(drivers: impl IntoIterator<Item = Arc<dyn ComputeDriver>>) -> Result<Self> {
        let mut by_id = BTreeMap::new();
        for driver in drivers {
            let id = driver.id().to_string();
            if by_id.insert(id.clone(), driver).is_some() {
                return Err(ComputeError::Configuration(format!(
                    "duplicate compute driver `{id}`"
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

    pub fn get(&self, id: &str) -> Result<Arc<dyn ComputeDriver>> {
        self.drivers
            .get(id)
            .cloned()
            .ok_or_else(|| ComputeError::ProviderNotFound(id.to_string()))
    }

    pub fn for_gateway(&self, deployment_id: &str) -> Result<Arc<dyn ComputeDriver>> {
        self.drivers
            .values()
            .find(|driver| deployment_id.starts_with(driver.gateway_prefix()))
            .cloned()
            .ok_or_else(|| ComputeError::ProviderNotFound(deployment_id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    struct Stub;

    #[async_trait]
    impl ComputeDriver for Stub {
        fn id(&self) -> &'static str {
            "stub"
        }
        fn gateway_prefix(&self) -> &'static str {
            "stub-"
        }
        fn capabilities(&self) -> DriverCapabilities {
            DriverCapabilities {
                provider: self.id().into(),
                display_name: "Stub".into(),
                source_kinds: vec![],
                operations: vec![],
                max_timeout_seconds: 1,
                default_region: "test".into(),
                notes: vec![],
            }
        }
        async fn deploy(&self, _: &Tenant, _: DeployRequest) -> Result<ComputeOperation> {
            unreachable!()
        }
        async fn get(&self, _: &Tenant, _: ResourceRequest) -> Result<Deployment> {
            unreachable!()
        }
        async fn list(&self, _: &Tenant, _: ListRequest) -> Result<DeploymentList> {
            unreachable!()
        }
        async fn delete(&self, _: &Tenant, _: ResourceRequest) -> Result<ComputeOperation> {
            unreachable!()
        }
        async fn operation(&self, _: &Tenant, _: &str) -> Result<ComputeOperation> {
            unreachable!()
        }
        async fn invoke(&self, _: &Tenant, _: InvokeRequest) -> Result<InvocationResult> {
            unreachable!()
        }
        async fn invoke_gateway(&self, _: GatewayInvokeRequest) -> Result<GatewayInvocation> {
            unreachable!()
        }
        async fn cleanup_channel(&self, _: &str) -> Result<usize> {
            Ok(0)
        }
    }

    #[test]
    fn registry_selects_by_stable_driver_id() {
        let registry = DriverRegistry::new([Arc::new(Stub) as Arc<dyn ComputeDriver>]).unwrap();
        assert_eq!(registry.get("stub").unwrap().id(), "stub");
        assert!(matches!(
            registry.get("missing"),
            Err(ComputeError::ProviderNotFound(_))
        ));
    }
}
