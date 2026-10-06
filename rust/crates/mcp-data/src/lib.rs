//! Provider-neutral control and data planes for managed data services.

pub mod binding;
pub mod driver;
pub mod firestore;
pub mod server;
pub mod types;

pub use driver::{DataDriver, DriverRegistry};
pub use server::DataMcp;
