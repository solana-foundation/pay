//! x402 protocol support.
//!
//! Thin wrapper around `solana_x402::client::solana` for challenge detection
//! and payment building.

use solana_x402::client::solana::{build_payment_header, parse_x402_challenge};
use solana_x402::protocol::methods::solana::{PaymentRequirements, default_rpc_url};
use solana_x402::solana_keychain::SolanaSigner;
use solana_x402::solana_rpc_client::rpc_client::RpcClient;
use tracing::info;

use crate::{Error, Result};

// Re-export for the runner/CLI.
pub use solana_x402::protocol::methods::solana::PaymentRequirements as Challenge;

/// Try to parse an x402 challenge from headers and/or body.
pub fn parse(headers: &[(String, String)], body: Option<&str>) -> Option<PaymentRequirements> {
    parse_x402_challenge(headers, body)
}

/// Build a signed payment and return the `X-PAYMENT` header value.
pub fn build_payment(requirements: &PaymentRequirements, keypair_source: &str) -> Result<String> {
    let amount = format_amount(&requirements.amount, &requirements.currency);
    let desc = requirements.description.as_deref().unwrap_or("API access");
    let reason = format!("pay {amount} for {desc}");

    let signer = crate::signer::load_signer_with_reason(keypair_source, &reason)?;

    let cluster = requirements.cluster.as_deref().unwrap_or("mainnet-beta");
    let rpc_url = std::env::var("PAY_RPC_URL")
        .unwrap_or_else(|_| default_rpc_url(cluster).to_string());
    let rpc = RpcClient::new(rpc_url.clone());

    info!(
        amount = %requirements.amount,
        currency = %requirements.currency,
        cluster,
        signer = %signer.pubkey(),
        "Building x402 payment"
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Mpp(format!("Failed to create runtime: {e}")))?;

    rt.block_on(build_payment_header(&signer, &rpc, requirements))
        .map_err(|e| Error::Mpp(format!("Failed to build x402 payment: {e}")))
}

fn format_amount(amount: &str, currency: &str) -> String {
    let base: u64 = amount.parse().unwrap_or(0);
    let value = if currency.to_uppercase() == "SOL" {
        base as f64 / 1_000_000_000.0
    } else {
        base as f64 / 1_000_000.0
    };
    format!("${}", format_value(value))
}

fn format_value(v: f64) -> String {
    if v == 0.0 {
        "0".to_string()
    } else if v >= 0.01 {
        format!("{v:.2}")
    } else if v >= 0.001 {
        format!("{v:.3}")
    } else if v >= 0.0001 {
        format!("{v:.4}")
    } else {
        format!("{v:.6}")
    }
}
