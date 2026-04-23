//! x402 protocol support.
//!
//! Thin wrapper around `solana_x402::client::solana` for challenge detection
//! and payment building.

use solana_x402::client::solana::build_payment as build_payment_payload;
use solana_x402::client::solana::build_payment_header as build_payment_header_v2;
use solana_x402::protocol::methods::solana::{PaymentRequirements, default_rpc_url};
use solana_x402::solana_keychain::SolanaSigner;
use solana_x402::solana_rpc_client::rpc_client::RpcClient;
use tracing::{info, warn};

use crate::accounts::{AccountsStore, ResolvedEphemeral};
use crate::{Error, Result};

pub const X402_V1_PAYMENT_HEADER: &str = "X-PAYMENT";
pub const X402_V2_PAYMENT_HEADER: &str = "PAYMENT-SIGNATURE";

#[derive(Debug, Clone)]
pub struct Challenge {
    pub x402_version: u8,
    pub requirements: PaymentRequirements,
    pub accepted: Option<serde_json::Value>,
    pub resource: Option<serde_json::Value>,
}

/// Try to parse an x402 challenge from headers and/or body.
/// Defaults to preferring Solana mainnet when multiple chains are offered.
pub fn parse(headers: &[(String, String)], body: Option<&str>) -> Option<Challenge> {
    use solana_x402::client::solana::parse_x402_challenge_for_network;
    let normalized_headers = normalize_x402_headers(headers);
    let decoded_payment_required = decoded_payment_required_body(headers);
    let parse_body = body.or(decoded_payment_required.as_deref());
    // Prefer mainnet; the CAIP-2 ID is resolved downstream by normalize_network.
    let requirements = parse_x402_challenge_for_network(
        &normalized_headers,
        parse_body,
        Some("solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp"),
    )?;
    let (accepted, resource) = extract_v2_context(parse_body, &requirements);
    Some(Challenge {
        x402_version: detect_x402_version(headers, body),
        requirements,
        accepted,
        resource,
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
/// 2. `requirements.cluster`.
/// 3. `mainnet`.
pub fn build_payment(
    challenge: &Challenge,
    store: &dyn AccountsStore,
    network_override: Option<&str>,
    account_override: Option<&str>,
) -> Result<(&'static str, String, Option<ResolvedEphemeral>)> {
    let requirements = &challenge.requirements;
    let amount = format_amount(&requirements.amount, &requirements.currency);
    let desc = requirements.description.as_deref().unwrap_or("API access");

    let cluster = normalize_network(requirements.cluster.as_deref().unwrap_or("mainnet"));

    // Client-side network intent check (same shape as mpp.rs). x402's
    // PaymentRequirements doesn't carry a `recentBlockhash` field, so
    // only the slug-mismatch branch can fire here — but we route
    // through the same helper for consistency.
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
        desc,
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
        1 => {
            let payload = rt
                .block_on(build_payment_payload(&signer, &rpc, requirements))
                .map_err(|e| Error::Mpp(format!("Failed to build x402 payment: {e}")))?;
            (X402_V1_PAYMENT_HEADER, encode_v1_payment_header(&payload, requirements)?)
        }
        _ => {
            let header = rt
                .block_on(build_payment_header_v2_with_context(
                    &signer,
                    &rpc,
                    requirements,
                    challenge.accepted.as_ref(),
                    challenge.resource.as_ref(),
                ))
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
        "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp" => "mainnet".to_string(),
        "solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1" => "devnet".to_string(),
        "solana:4uhcVJyU9pJkvQyS88uRDiswHXSCkY3z" => "testnet".to_string(),
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

fn detect_x402_version(headers: &[(String, String)], body: Option<&str>) -> u8 {
    if headers.iter().any(|(k, _)| k == "payment-required") {
        return 2;
    }

    if let Some(body) = body {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
            if let Some(version) = json.get("x402Version").and_then(|v| v.as_u64()) {
                return version as u8;
            }
        }
    }

    if headers.iter().any(|(k, _)| k == "x-payment-required") {
        return 1;
    }

    2
}

fn normalize_x402_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut normalized = headers.to_vec();

    if normalized.iter().any(|(k, _)| k == "x-payment-required") {
        return normalized;
    }

    if let Some(json) = decoded_payment_required_body(headers) {
        normalized.push(("x-payment-required".to_string(), json));
    }

    normalized
}

fn decoded_payment_required_body(headers: &[(String, String)]) -> Option<String> {
    let (_, value) = headers.iter().find(|(k, _)| k == "payment-required")?;
    let decoded =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, value).ok()?;
    String::from_utf8(decoded).ok()
}

fn encode_v1_payment_header(
    payload: &solana_x402::protocol::methods::solana::PaymentPayload,
    requirements: &PaymentRequirements,
) -> Result<String> {
    let transaction = match &payload.proof {
        solana_x402::protocol::methods::solana::PaymentProof::Transaction { transaction } => {
            transaction.clone()
        }
        solana_x402::protocol::methods::solana::PaymentProof::Signature { signature } => {
            signature.clone()
        }
    };

    let network = match requirements.cluster.as_deref() {
        Some("devnet") | Some("solana-devnet") | Some("solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1") => {
            "solana-devnet"
        }
        _ => "solana",
    };

    let envelope = serde_json::json!({
        "x402Version": 1,
        "scheme": "exact",
        "network": network,
        "payload": {
            "transaction": transaction,
        }
    });

    let json = serde_json::to_string(&envelope)
        .map_err(|e| Error::Mpp(format!("JSON serialization failed: {e}")))?;
    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        json.as_bytes(),
    ))
}

async fn build_payment_header_v2_with_context(
    signer: &dyn SolanaSigner,
    rpc: &RpcClient,
    requirements: &PaymentRequirements,
    accepted: Option<&serde_json::Value>,
    resource: Option<&serde_json::Value>,
) -> std::result::Result<String, solana_x402::error::Error> {
    let header = if accepted.is_none() && resource.is_none() {
        build_payment_header_v2(signer, rpc, requirements).await?
    } else {
        let payload = build_payment_payload(signer, rpc, requirements).await?;
        let tx_base64 = match &payload.proof {
            solana_x402::protocol::methods::solana::PaymentProof::Transaction { transaction } => {
                transaction.clone()
            }
            solana_x402::protocol::methods::solana::PaymentProof::Signature { signature } => {
                signature.clone()
            }
        };

        let accepted = accepted.cloned().unwrap_or_else(|| {
            serde_json::json!({
                "scheme": "exact",
                "network": requirements.cluster.as_deref().unwrap_or("solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp"),
                "amount": requirements.amount,
                "asset": requirements.currency,
                "payTo": requirements.recipient,
                "maxTimeoutSeconds": requirements.max_age.unwrap_or(300),
                "extra": {
                    "feePayer": requirements.fee_payer_key,
                }
            })
        });

        let envelope = serde_json::json!({
            "x402Version": 2,
            "payload": {
                "transaction": tx_base64,
            },
            "accepted": accepted,
            "resource": resource.cloned(),
        });

        base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            serde_json::to_string(&envelope)
                .map_err(|e| solana_x402::error::Error::Other(format!("JSON serialization failed: {e}")))?
                .as_bytes(),
        )
    };

    Ok(header)
}

fn extract_v2_context(
    body: Option<&str>,
    requirements: &PaymentRequirements,
) -> (Option<serde_json::Value>, Option<serde_json::Value>) {
    let Some(body) = body else {
        return (None, None);
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return (None, None);
    };

    let resource = json.get("resource").cloned();
    let accepted = json
        .get("accepts")
        .and_then(|v| v.as_array())
        .and_then(|accepts| {
            accepts.iter().find(|accept| {
                let pay_to = accept.get("payTo").and_then(|v| v.as_str());
                let asset = accept.get("asset").and_then(|v| v.as_str());
                let amount = accept
                    .get("amount")
                    .or_else(|| accept.get("maxAmountRequired"))
                    .and_then(|v| v.as_str());
                pay_to == Some(requirements.recipient.as_str())
                    && asset == Some(requirements.currency.as_str())
                    && amount == Some(requirements.amount.as_str())
            })
        })
        .cloned();

    (accepted, resource)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::MemoryAccountsStore;

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
        let body = r#"{
            "x402Version": 1,
            "accepts": [{
                "network": "solana-devnet",
                "maxAmountRequired": "5000",
                "payTo": "abc123",
                "asset": "SOL",
                "resource": "/test"
            }]
        }"#;

        let challenge = parse(&[], Some(body)).unwrap();
        assert_eq!(challenge.x402_version, 1);
        assert_eq!(challenge.requirements.amount, "5000");
    }

    #[test]
    fn parse_v2_payment_required_header_sets_v2_version() {
        let payment_required = serde_json::json!({
            "x402Version": 2,
            "accepts": [{
                "scheme": "exact",
                "network": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
                "amount": "10000",
                "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "payTo": "6cvgmdrsVxyiuPzqMCSBnS7fAmA5Mk2VG4BcfVhC8jdC",
                "maxTimeoutSeconds": 300,
                "extra": {
                    "feePayer": "AepWpq3GQwL8CeKMtZyKtKPa7W91Coygh3ropAJapVdU",
                    "decimals": 6
                }
            }]
        });
        let encoded = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            payment_required.to_string().as_bytes(),
        );
        let headers = vec![("payment-required".to_string(), encoded)];

        let challenge = parse(&headers, None).unwrap();
        assert_eq!(challenge.x402_version, 2);
        assert_eq!(challenge.requirements.amount, "10000");
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
            x402_version: 2,
            requirements: sample_requirements(),
            accepted: None,
            resource: None,
        };

        let err = build_payment(&challenge, &store, Some("localnet"), None).unwrap_err();
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
            x402_version: 2,
            requirements: sample_requirements(),
            accepted: None,
            resource: None,
        };

        let err = build_payment(&challenge, &store, None, None).unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains("No account configured for network `mainnet`"));
        assert!(msg.contains("pay setup"));
    }

    #[test]
    fn build_payment_reports_named_account_miss_for_network() {
        let store = MemoryAccountsStore::new();
        let requirements = sample_requirements();

        let challenge = Challenge {
            x402_version: 2,
            requirements,
            accepted: None,
            resource: None,
        };
        let err = build_payment(&challenge, &store, None, Some("alice")).unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains("No account named `alice` configured for network `mainnet`"));
        assert_eq!(
            store.save_count(),
            0,
            "named-account miss must not lazily create"
        );
    }
}
