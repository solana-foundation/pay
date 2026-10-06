//! Provider-neutral cleanup for resources leased to an MPP channel.

use std::sync::Arc;

use mcp_compute::ComputeDriver;
use mcp_compute::google::{GoogleCloudFunctionsDriver, GoogleConfig};
use mcp_data::DataDriver;
use mcp_data::firestore::{FirestoreConfig, FirestoreDriver};

use crate::error::JobError;

const COMPUTE_DRIVER: &str = "google-cloud-functions";
const DATA_DRIVER: &str = "gcp-firestore";

#[derive(Default)]
pub struct ResourceCleaner {
    compute: Vec<Arc<dyn ComputeDriver>>,
    data: Vec<Arc<dyn DataDriver>>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CleanupSummary {
    pub compute_resources: usize,
    pub data_resources: usize,
}

impl ResourceCleaner {
    /// Build exactly the configured provider drivers. An unknown or
    /// misconfigured driver aborts startup so production cannot silently leak
    /// resources after a channel becomes unusable.
    pub fn from_env() -> Result<Self, JobError> {
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
        !self.compute.is_empty() || !self.data.is_empty()
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
