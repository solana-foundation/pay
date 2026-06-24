// Shared modules
pub mod accounts;
pub mod config;
pub mod error;
pub mod explorer;
pub mod instructions;
pub mod keystore;
pub mod signer;
pub mod skills;

// Client modules (CLI)
pub mod client;

// Flat re-exports so callers can use `pay_core::mpp`, `pay_core::runner`, etc.
pub use client::balance;
pub use client::fetch;
pub use client::mpp;
pub use client::runner;
pub use client::runner::{
    run_curl, run_curl_with_headers, run_httpie, run_httpie_with_headers, run_wget,
    run_wget_with_headers,
};
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
pub use pay_kit::mpp as solana_mpp;
#[cfg(feature = "server")]
use pay_kit::mpp::server::Mpp;
#[cfg(feature = "server")]
use pay_types::metering::ApiSpec;
#[cfg(feature = "server")]
use std::sync::Arc;

/// Trait that the application state must implement for the payment middleware.
#[cfg(feature = "server")]
pub trait PaymentState: Clone + Send + Sync + 'static {
    fn apis(&self) -> &[ApiSpec];
    fn mpp(&self) -> Option<&Mpp>;
    fn mpps(&self) -> Vec<&Mpp> {
        self.mpp().into_iter().collect()
    }
    fn browser_rpc_url(&self) -> Option<&str> {
        None
    }
    fn session_mpp(&self) -> Option<&server::session::SessionMpp> {
        None
    }
    fn session_mpp_handle(&self) -> Option<Arc<server::session::SessionMpp>> {
        None
    }
    fn fee_payer_wallet(&self) -> Option<&server::telemetry::FeePayerWallet> {
        None
    }
    /// Operator's fee-payer signer, when configured. The subscription
    /// middleware needs it at verify time to co-sign the activation
    /// transaction; charge / session paths construct their own MPP
    /// instances at startup and don't ask for it through this trait.
    fn fee_payer_signer(&self) -> Option<Arc<dyn pay_kit::mpp::solana_keychain::SolanaSigner>> {
        None
    }

    /// x402 `exact` handler, when the server accepts x402 payments.
    fn x402(&self) -> Option<&pay_kit::x402::server::X402> {
        None
    }
    /// x402 `upto` (usage-based) handler, when configured with an operator signer.
    fn x402_upto(&self) -> Option<&pay_kit::x402::server::X402Upto> {
        None
    }
    /// x402 `batch-settlement` handler, when configured with an operator signer.
    fn x402_batch(&self) -> Option<&pay_kit::x402::server::X402BatchSettlement> {
        None
    }
}
