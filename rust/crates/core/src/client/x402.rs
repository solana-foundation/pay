//! x402 protocol support.
//!
//! Thin wrapper around `solana_x402::client::solana` for challenge detection
//! and payment building.

use solana_x402::client::solana::{build_payment_header, parse_x402_challenge};
use solana_x402::protocol::methods::solana::{PaymentRequirements, default_rpc_url};
use solana_x402::solana_keychain::SolanaSigner;
use solana_x402::solana_rpc_client::rpc_client::RpcClient;
use tracing::{info, warn};

use crate::accounts::{AccountsStore, ResolvedEphemeral};
use crate::{Error, Result};

// Re-export for the runner/CLI.
pub use solana_x402::protocol::methods::solana::PaymentRequirements as Challenge;

/// Try to parse an x402 challenge from headers and/or body.
pub fn parse(headers: &[(String, String)], body: Option<&str>) -> Option<PaymentRequirements> {
    parse_x402_challenge(headers, body)
}

/// Build a signed payment and return `(X-PAYMENT header, ephemeral_notice)`.
///
/// The `ephemeral_notice` is `Some` only when this call generated a fresh
/// ephemeral wallet — the caller renders the "Generated <network> wallet"
/// CLI notice with it.
///
/// Network resolution mirrors `mpp::build_credential`:
/// 1. `network_override` (CLI flag) wins.
/// 2. `requirements.cluster`.
/// 3. `mainnet`.
pub fn build_payment(
    requirements: &PaymentRequirements,
    store: &dyn AccountsStore,
    network_override: Option<&str>,
) -> Result<(String, Option<ResolvedEphemeral>)> {
    let amount = format_amount(&requirements.amount, &requirements.currency);
    let desc = requirements.description.as_deref().unwrap_or("API access");

    let cluster = requirements
        .cluster
        .as_deref()
        .unwrap_or("mainnet")
        .to_string();

    // Client-side network intent check (same shape as mpp.rs). x402's
    // PaymentRequirements doesn't carry a `recentBlockhash` field, so
    // only the slug-mismatch branch can fire here — but we route
    // through the same helper for consistency.
    crate::client::mpp::check_client_network_intent(network_override, &cluster, None)?;

    // Same gating rationale as mpp::build_credential — only attempt
    // auto-funding when the user explicitly opted into sandbox mode.
    let user_opted_into_sandbox = network_override.is_some();
    let network = network_override.map(str::to_string).unwrap_or(cluster);

    let (signer, ephemeral_notice) =
        crate::signer::load_signer_for_network_payment(&network, store, &amount, desc)?;

    let rpc_url =
        std::env::var("PAY_RPC_URL").unwrap_or_else(|_| default_rpc_url(&network).to_string());
    let rpc = RpcClient::new(rpc_url.clone());

    info!(
        amount = %requirements.amount,
        currency = %requirements.currency,
        cluster = %network,
        signer = %signer.pubkey(),
        "Building x402 payment"
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Mpp(format!("Failed to create runtime: {e}")))?;

    if user_opted_into_sandbox
        && ephemeral_notice
            .as_ref()
            .map(|e| e.created)
            .unwrap_or(false)
    {
        let pubkey = signer.pubkey().to_string();
        let fund_url = rpc_url.clone();
        if let Err(e) = rt.block_on(crate::client::sandbox::fund_via_surfpool(
            &fund_url, &pubkey,
        )) {
            warn!(error = %e, "Could not auto-fund ephemeral via Surfpool — broadcast may fail if wallet is empty");
        }
    }

    let header = rt
        .block_on(build_payment_header(&signer, &rpc, requirements))
        .map_err(|e| Error::Mpp(format!("Failed to build x402 payment: {e}")))?;

    Ok((header, ephemeral_notice))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_value_zero() {
        assert_eq!(format_value(0.0), "0");
    }

    #[test]
    fn format_value_large() {
        assert_eq!(format_value(1.5), "1.50");
    }

    #[test]
    fn format_value_cents() {
        assert_eq!(format_value(0.01), "0.01");
    }

    #[test]
    fn format_value_milli() {
        assert_eq!(format_value(0.005), "0.005");
    }

    #[test]
    fn format_value_micro() {
        assert_eq!(format_value(0.0005), "0.0005");
    }

    #[test]
    fn format_value_tiny() {
        assert_eq!(format_value(0.00005), "0.000050");
    }

    #[test]
    fn format_amount_usdc() {
        assert_eq!(format_amount("1000000", "USDC"), "$1.00");
    }

    #[test]
    fn format_amount_sol() {
        assert_eq!(format_amount("1000000000", "SOL"), "$1.00");
    }

    #[test]
    fn format_amount_zero() {
        assert_eq!(format_amount("0", "USDC"), "$0");
    }

    #[test]
    fn format_amount_invalid_number() {
        assert_eq!(format_amount("abc", "USDC"), "$0");
    }

    #[test]
    fn parse_empty_headers_and_body() {
        assert!(parse(&[], None).is_none());
    }

    #[test]
    fn parse_no_x402_headers() {
        let headers = vec![("content-type".to_string(), "text/html".to_string())];
        assert!(parse(&headers, None).is_none());
    }
}
