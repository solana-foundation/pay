//! MPP (Machine Payments Protocol) support.
//!
//! Thin wrapper around `solana_mpp::client::solana` for challenge detection
//! and credential building.

use solana_mpp::client::solana::{build_credential_header, parse_www_authenticate};
use solana_mpp::protocol::methods::solana::default_rpc_url;
use solana_mpp::solana_keychain::SolanaSigner;
use solana_mpp::solana_rpc_client::rpc_client::RpcClient;
use tracing::info;

use crate::{Error, Result};

// Re-export the challenge type for the runner/CLI.
pub type Challenge = solana_mpp::protocol::methods::solana::MppChallenge;

/// Try to extract an MPP challenge from the `www-authenticate` header value.
pub fn parse(header_value: &str) -> Option<Challenge> {
    parse_www_authenticate(header_value)
}

/// Build a signed credential and return the `Authorization` header value.
pub fn build_credential(challenge: &Challenge, keypair_source: &str) -> Result<String> {
    let amount = format_amount(&challenge.request.amount, &challenge.request.currency);
    let desc = challenge.description.as_deref().unwrap_or("API access");
    let reason = format!("pay {amount} for {desc}");

    let signer = crate::signer::load_signer_with_reason(keypair_source, &reason)?;

    let network = challenge
        .request
        .method_details
        .network
        .as_deref()
        .unwrap_or("mainnet-beta");
    let rpc_url =
        std::env::var("PAY_RPC_URL").unwrap_or_else(|_| default_rpc_url(network).to_string());
    let rpc = RpcClient::new(rpc_url.clone());

    info!(
        amount = %challenge.request.amount,
        currency = %challenge.request.currency,
        network,
        signer = %signer.pubkey(),
        "Building MPP credential"
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Mpp(format!("Failed to create runtime: {e}")))?;

    rt.block_on(build_credential_header(&signer, &rpc, challenge))
        .map_err(|e| Error::Mpp(format!("Failed to build credential: {e}")))
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
