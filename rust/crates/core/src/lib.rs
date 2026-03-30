// Shared modules
pub mod accounts;
pub mod config;
pub mod error;
pub mod keystore;
pub mod signer;

// Client modules (CLI)
pub mod client;

// Server modules (gateway proxy)
pub mod server;

pub use config::{Config, LogFormat};
pub use error::{Error, Result};
pub use server::{AccountingKey, AccountingStore, InMemoryStore, current_period};

#[cfg(feature = "server")]
pub use solana_mpp;
#[cfg(feature = "server")]
use pay_types::metering::ApiSpec;
#[cfg(feature = "server")]
use solana_mpp::server::Mpp;

/// Trait that the application state must implement for the payment middleware.
#[cfg(feature = "server")]
pub trait PaymentState: Clone + Send + Sync + 'static {
    fn apis(&self) -> &[ApiSpec];
    fn mpp(&self) -> Option<&Mpp>;
}
