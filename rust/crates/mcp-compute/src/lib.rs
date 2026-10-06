//! Provider-neutral MCP control plane for serverless workloads and triggers.

pub mod binding;
pub mod driver;
pub mod google;
pub mod server;
pub mod trigger_driver;
pub mod trigger_google;
pub mod trigger_types;
pub mod types;

pub use driver::{ComputeDriver, DriverRegistry};
pub use server::ComputeMcp;
pub use trigger_driver::{TriggerDriver, TriggerDriverRegistry};
