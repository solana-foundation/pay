//! x402 protocol support.
//!
//! Thin wrapper around `solana_x402::client::exact` for challenge detection
//! and payment building.

use solana_x402::solana_keychain::SolanaSigner;
use solana_x402::solana_rpc_client::rpc_client::RpcClient;
use solana_x402::{
    PAYMENT_REQUIRED_HEADER, X402_V1_PAYMENT_REQUIRED_HEADER, X402_VERSION_FIELD, X402_VERSION_V1,
    X402_VERSION_V2,
    client::exact::{
        build_payment_header as build_payment_header_v2, build_payment_header_v1,
        parse_x402_challenge_for_network,
    },
    exact::{PaymentRequirements, SOLANA_DEVNET, SOLANA_MAINNET, SOLANA_TESTNET, default_rpc_url},
};
use tracing::{info, warn};

use crate::accounts::{AccountsStore, ResolvedEphemeral};
use crate::{Error, Result};

pub use solana_x402::{X402_V1_PAYMENT_HEADER, X402_V2_PAYMENT_HEADER};

#[derive(Debug, Clone)]
pub struct Challenge {
    pub x402_version: u64,
    pub requirements: PaymentRequirements,
}

/// Try to parse an x402 challenge from headers and/or body.
/// Defaults to preferring Solana mainnet when multiple chains are offered.
pub fn parse(headers: &[(String, String)], body: Option<&str>) -> Option<Challenge> {
    let requirements = parse_x402_challenge_for_network(headers, body, Some(SOLANA_MAINNET))?;
    Some(Challenge {
        x402_version: detect_x402_version(headers, body),
        requirements,
    })
}

/// Build a signed payment and return `(header_name, header_value, ephemeral_notice)`.
///
/// The `ephemeral_notice` is `Some` only when this call generated a fresh
/// ephemeral wallet — the caller renders the "Generated <network> wallet"
/// CLI notice with it.
///
/// Network resolution mirrors `mpp::build_credential`:
/// 1. `network_override` (CLI flag) wins.
/// 2. `requirements.cluster` or `requirements.network`.
/// 3. `mainnet`.
pub fn build_payment(
    challenge: &Challenge,
    store: &dyn AccountsStore,
    network_override: Option<&str>,
    account_override: Option<&str>,
    resource_url: Option<&str>,
) -> Result<(&'static str, String, Option<ResolvedEphemeral>)> {
    let requirements = &challenge.requirements;
    let amount = format_amount(&requirements.amount, &requirements.currency);
    let desc = crate::client::prompt::payment_description(
        requirements.description.as_deref(),
        &[Some(requirements.resource.as_str()), resource_url],
    );

    let cluster = normalize_network(
        requirements
            .cluster
            .as_deref()
            .unwrap_or(requirements.network.as_str()),
    );

    // x402 may carry a recent blockhash, but the current pay-side guard only
    // compares the selected account network against the challenge network.
    crate::client::mpp::check_client_network_intent(network_override, &cluster, None)?;

    // Auto-fund when the user opted into sandbox or the challenge
    // advertises localnet (likely a sandbox gateway without --sandbox).
    let user_opted_into_sandbox = network_override.is_some() || cluster == "localnet";
    let network = network_override.map(str::to_string).unwrap_or(cluster);

    let (signer, ephemeral_notice) = crate::signer::load_signer_for_network_payment(
        &network,
        store,
        account_override,
        &amount,
        &desc,
    )?;

    let rpc_url =
        std::env::var("PAY_RPC_URL").unwrap_or_else(|_| default_rpc_url(&network).to_string());
    let rpc = RpcClient::new(rpc_url.clone());

    info!(
        amount = %requirements.amount,
        currency = %requirements.currency,
        cluster = %network,
        recipient = %requirements.recipient,
        signer = %signer.pubkey(),
        "Building x402 payment"
    );
    tracing::debug!(?requirements, "Full x402 requirements");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Mpp(format!("Failed to create runtime: {e}")))?;

    if user_opted_into_sandbox {
        let pubkey = signer.pubkey().to_string();
        let fund_url = rpc_url.clone();
        if let Err(e) = rt.block_on(crate::client::sandbox::fund_via_surfpool(
            &fund_url, &pubkey,
        )) {
            warn!(error = %e, "Could not auto-fund ephemeral via Surfpool — broadcast may fail if wallet is empty");
        }
    }

    let (header_name, header_value) = match challenge.x402_version {
        X402_VERSION_V1 => {
            let header = rt
                .block_on(build_payment_header_v1(&signer, &rpc, requirements))
                .map_err(|e| Error::Mpp(format!("Failed to build x402 payment: {e}")))?;
            (X402_V1_PAYMENT_HEADER, header)
        }
        _ => {
            let header = rt
                .block_on(build_payment_header_v2(&signer, &rpc, requirements))
                .map_err(|e| Error::Mpp(format!("Failed to build x402 payment: {e}")))?;
            (X402_V2_PAYMENT_HEADER, header)
        }
    };

    Ok((header_name, header_value, ephemeral_notice))
}

/// Normalize CAIP-2 network identifiers to the slugs pay uses internally.
///
/// x402 challenges use CAIP-2 chain IDs like `solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp`
/// (Solana mainnet). The pay account system uses `mainnet`, `devnet`, `localnet`.
fn normalize_network(raw: &str) -> String {
    match raw {
        // Solana CAIP-2 genesis hashes
        SOLANA_MAINNET | "solana" | "mainnet-beta" => "mainnet".to_string(),
        SOLANA_DEVNET | "solana-devnet" => "devnet".to_string(),
        SOLANA_TESTNET | "solana-testnet" => "testnet".to_string(),
        // Already a slug
        s if !s.contains(':') => s.to_string(),
        // Unknown CAIP-2 — pass through, will error downstream with a clear message
        other => other.to_string(),
    }
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

fn detect_x402_version(headers: &[(String, String)], body: Option<&str>) -> u64 {
    if header_value(headers, PAYMENT_REQUIRED_HEADER).is_some() {
        return X402_VERSION_V2;
    }

    if let Some(version) = body.and_then(x402_version_from_json) {
        return version;
    }

    if let Some(value) = header_value(headers, X402_V1_PAYMENT_REQUIRED_HEADER) {
        return x402_version_from_json(value).unwrap_or(X402_VERSION_V1);
    }

    X402_VERSION_V2
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn x402_version_from_json(body: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get(X402_VERSION_FIELD)
        .and_then(|v| v.as_u64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::MemoryAccountsStore;
    use solana_x402::exact::EXACT_SCHEME;

    fn sample_requirements() -> PaymentRequirements {
        PaymentRequirements {
            network: "solana".to_string(),
            cluster: Some("mainnet".to_string()),
            recipient: "11111111111111111111111111111111".to_string(),
            amount: "1000000".to_string(),
            currency: "USDC".to_string(),
            decimals: Some(6),
            token_program: None,
            resource: "https://api.example.com/v1/test".to_string(),
            description: Some("API access".to_string()),
            max_age: Some(60),
            recent_blockhash: None,
            fee_payer: None,
            fee_payer_key: None,
            extra: None,
            accepted: None,
            resource_info: None,
        }
    }

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

    #[test]
    fn parse_v1_body_sets_v1_version() {
        let body = serde_json::json!({
            X402_VERSION_FIELD: X402_VERSION_V1,
            "accepts": [{
                "network": "solana-devnet",
                "maxAmountRequired": "5000",
                "payTo": "abc123",
                "asset": "SOL",
                "resource": "/test"
            }]
        })
        .to_string();

        let challenge = parse(&[], Some(&body)).unwrap();
        assert_eq!(challenge.x402_version, X402_VERSION_V1);
        assert_eq!(challenge.requirements.amount, "5000");
    }

    #[test]
    fn parse_v2_payment_required_header_sets_v2_version() {
        let selected = serde_json::json!({
            "scheme": EXACT_SCHEME,
            "network": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
            "amount": "10000",
            "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "payTo": "6cvgmdrsVxyiuPzqMCSBnS7fAmA5Mk2VG4BcfVhC8jdC",
            "maxTimeoutSeconds": 300,
            "extra": {
                "feePayer": "AepWpq3GQwL8CeKMtZyKtKPa7W91Coygh3ropAJapVdU",
                "decimals": 6
            }
        });
        let payment_required = serde_json::json!({
            X402_VERSION_FIELD: X402_VERSION_V2,
            "resource": {
                "url": "https://api.example.com/v1/test",
                "description": "API access"
            },
            "accepts": [selected.clone()]
        });
        let encoded = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            payment_required.to_string().as_bytes(),
        );
        let headers = vec![(PAYMENT_REQUIRED_HEADER.to_string(), encoded)];

        let challenge = parse(&headers, None).unwrap();
        assert_eq!(challenge.x402_version, X402_VERSION_V2);
        assert_eq!(challenge.requirements.amount, "10000");
        assert_eq!(challenge.requirements.accepted.as_ref(), Some(&selected));
        assert_eq!(
            challenge
                .requirements
                .resource_info
                .as_ref()
                .map(|resource| resource.url.as_str()),
            Some("https://api.example.com/v1/test")
        );
    }

    #[test]
    fn normalize_network_maps_sdk_identifiers_to_pay_slugs() {
        assert_eq!(normalize_network(SOLANA_MAINNET), "mainnet");
        assert_eq!(normalize_network("mainnet-beta"), "mainnet");
        assert_eq!(normalize_network(SOLANA_DEVNET), "devnet");
        assert_eq!(normalize_network("solana-devnet"), "devnet");
        assert_eq!(normalize_network(SOLANA_TESTNET), "testnet");
        assert_eq!(normalize_network("localnet"), "localnet");
    }

    #[test]
    fn v1_header_name_is_x_payment() {
        assert_eq!(X402_V1_PAYMENT_HEADER, "X-PAYMENT");
    }

    #[test]
    fn v2_header_name_is_payment_signature() {
        assert_eq!(X402_V2_PAYMENT_HEADER, "PAYMENT-SIGNATURE");
    }

    #[test]
    fn build_payment_rejects_network_intent_mismatch_before_signer_lookup() {
        let store = MemoryAccountsStore::new();
        let challenge = Challenge {
            x402_version: X402_VERSION_V2,
            requirements: sample_requirements(),
        };

        let err = build_payment(&challenge, &store, Some("localnet"), None, None).unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains("you forced network `localnet`"));
        assert!(msg.contains("server expects `mainnet`"));
        assert_eq!(
            store.save_count(),
            0,
            "mismatch must fail before any wallet mutation"
        );
    }

    #[test]
    fn build_payment_requires_mainnet_wallet_when_no_override_is_set() {
        let store = MemoryAccountsStore::new();
        let challenge = Challenge {
            x402_version: X402_VERSION_V2,
            requirements: sample_requirements(),
        };

        let err = build_payment(&challenge, &store, None, None, None).unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains("No account configured for network `mainnet`"));
        assert!(msg.contains("pay setup"));
    }

    #[test]
    fn build_payment_reports_named_account_miss_for_network() {
        let store = MemoryAccountsStore::new();
        let requirements = sample_requirements();

        let challenge = Challenge {
            x402_version: X402_VERSION_V2,
            requirements,
        };
        let err = build_payment(&challenge, &store, None, Some("alice"), None).unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains("No account named `alice` configured for network `mainnet`"));
        assert_eq!(
            store.save_count(),
            0,
            "named-account miss must not lazily create"
        );
    }
}
