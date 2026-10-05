//! Provider-neutral MCP control plane for short-lived serverless compute.

pub mod driver;
pub mod google;
pub mod server;
pub mod types;

pub use driver::{ComputeDriver, DriverRegistry};
pub use server::ComputeMcp;
