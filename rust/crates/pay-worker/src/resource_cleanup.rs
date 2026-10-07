//! Provider-neutral cleanup for resources leased to an MPP channel.

use std::sync::Arc;

use mcp_compute::google::{GoogleCloudFunctionsDriver, GoogleConfig};
use mcp_compute::trigger_google::{GoogleTriggerConfig, GoogleTriggerDriver};
use mcp_compute::{ComputeDriver, TriggerDriver};
use mcp_data::DataDriver;
use mcp_data::firestore::{FirestoreConfig, FirestoreDriver};

use crate::error::JobError;

const COMPUTE_DRIVER: &str = "google-cloud-functions";
const DATA_DRIVER: &str = "gcp-firestore";
const TRIGGER_DRIVER: &str = "google-cloud-scheduler";

#[derive(Default)]
pub struct ResourceCleaner {
    compute: Vec<Arc<dyn ComputeDriver>>,
    data: Vec<Arc<dyn DataDriver>>,
    triggers: Vec<Arc<dyn TriggerDriver>>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CleanupSummary {
    pub compute_resources: usize,
    pub data_resources: usize,
    pub trigger_resources: usize,
}

impl ResourceCleaner {
    /// The isolated orphan job deliberately supports only compute drivers and
    /// loads no financial, wallet, data, or trigger runtime.
    pub fn for_reconciliation() -> Result<Self, JobError> {
        let configured = std::env::var("PAY_RESOURCE_CLEANUP_DRIVERS")
            .map_err(|_| JobError::Config("PAY_RESOURCE_CLEANUP_DRIVERS is required".into()))?;
        let mut cleaner = Self::default();
        for driver in configured
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if driver != COMPUTE_DRIVER {
                return Err(JobError::Config(format!(
                    "unsupported orphan reconciliation driver: {driver}"
                )));
            }
            let config =
                GoogleConfig::from_env().map_err(|error| JobError::Config(error.to_string()))?;
            cleaner.compute.push(Arc::new(
                GoogleCloudFunctionsDriver::new(config)
                    .map_err(|error| JobError::Config(error.to_string()))?,
            ));
        }
        if cleaner.compute.is_empty() {
            return Err(JobError::Config(
                "at least one reconciliation driver is required".into(),
            ));
        }
        Ok(cleaner)
    }

    /// Failures are isolated from settlement by the separate scheduled job.
    pub async fn reconcile_orphans(&self, dry_run: bool) -> Result<usize, JobError> {
        let mut candidates = 0;
        let mut first_error = None;
        for driver in &self.compute {
            match driver.reconcile_orphans(dry_run).await {
                Ok(count) => candidates += count,
                Err(error) => {
                    tracing::error!(driver = driver.id(), %error, dry_run, "resource reconciliation failed");
                    first_error.get_or_insert_with(|| {
                        JobError::Config(format!("compute reconciliation: {error}"))
                    });
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(candidates),
        }
    }

    /// Build exactly the configured provider drivers. An unknown or
    /// misconfigured driver aborts startup so production cannot silently leak
    /// resources after a channel becomes unusable.
    pub async fn from_env() -> Result<Self, JobError> {
        let configured = std::env::var("PAY_RESOURCE_CLEANUP_DRIVERS").unwrap_or_default();
        let mut cleaner = Self::default();
        for driver in configured
            .split(',')
            .map(str::trim)
            .filter(|driver| !driver.is_empty())
        {
            match driver {
                COMPUTE_DRIVER => cleaner.compute.push(Arc::new(
                    GoogleCloudFunctionsDriver::new(
                        GoogleConfig::from_env()
                            .map_err(|error| JobError::Config(error.to_string()))?,
                    )
                    .map_err(|error| JobError::Config(error.to_string()))?,
                )),
                DATA_DRIVER => cleaner.data.push(Arc::new(
                    FirestoreDriver::new(
                        FirestoreConfig::from_env()
                            .map_err(|error| JobError::Config(error.to_string()))?,
                    )
                    .map_err(|error| JobError::Config(error.to_string()))?,
                )),
                TRIGGER_DRIVER => cleaner.triggers.push(Arc::new(
                    GoogleTriggerDriver::new(
                        GoogleTriggerConfig::from_env()
                            .map_err(|error| JobError::Config(error.to_string()))?,
                    )
                    .await
                    .map_err(|error| JobError::Config(error.to_string()))?,
                )),
                other => {
                    return Err(JobError::Config(format!(
                        "unknown PAY_RESOURCE_CLEANUP_DRIVERS entry: {other}"
                    )));
                }
            }
        }
        Ok(cleaner)
    }

    pub fn is_enabled(&self) -> bool {
        !self.compute.is_empty() || !self.data.is_empty() || !self.triggers.is_empty()
    }

    pub async fn cleanup_channel(&self, channel_id: &str) -> Result<CleanupSummary, JobError> {
        let mut summary = CleanupSummary::default();
        for driver in &self.compute {
            summary.compute_resources += driver
                .cleanup_channel(channel_id)
                .await
                .map_err(|error| JobError::Config(format!("compute cleanup: {error}")))?;
        }
        for driver in &self.data {
            summary.data_resources += driver
                .cleanup_channel(channel_id)
                .await
                .map_err(|error| JobError::Config(format!("data cleanup: {error}")))?;
        }
        for driver in &self.triggers {
            summary.trigger_resources += driver
                .cleanup_channel(channel_id)
                .await
                .map_err(|error| JobError::Config(format!("trigger cleanup: {error}")))?;
        }
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_driver_list_disables_cleanup() {
        let cleaner = ResourceCleaner::default();
        assert!(!cleaner.is_enabled());
    }
}
