// Shared modules
pub mod accounts;
pub mod bazaar;
pub mod config;
pub mod error;
pub mod instructions;
pub mod keystore;
pub mod signer;

// Client modules (CLI)
pub mod client;

// Flat re-exports so callers can use `pay_core::mpp`, `pay_core::runner`, etc.
pub use client::balance;
pub use client::fetch;
pub use client::mpp;
pub use client::runner;
pub use client::runner::{run_curl, run_curl_with_headers, run_wget, run_wget_with_headers};
pub use client::sandbox;
pub use client::send;
pub use client::session;
pub use client::x402;

// Server modules (gateway proxy)
pub mod server;

pub use config::{Config, LogFormat};
pub use error::{Error, Result};
pub use server::{AccountingKey, AccountingStore, InMemoryStore, current_period};

#[cfg(feature = "server")]
use pay_types::metering::ApiSpec;
#[cfg(feature = "server")]
pub use solana_mpp;
#[cfg(feature = "server")]
use solana_mpp::server::Mpp;

/// Trait that the application state must implement for the payment middleware.
#[cfg(feature = "server")]
pub trait PaymentState: Clone + Send + Sync + 'static {
    fn apis(&self) -> &[ApiSpec];
    fn mpp(&self) -> Option<&Mpp>;
    fn session_mpp(&self) -> Option<&server::session::SessionMpp> {
        None
    }
}
