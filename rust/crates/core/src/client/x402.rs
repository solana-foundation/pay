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
    exact::{
        PaymentRequiredEnvelope, PaymentRequirements, SOLANA_DEVNET, SOLANA_MAINNET,
        SOLANA_TESTNET, default_rpc_url,
    },
    siwx::{
        SiwxChainSelectionOptions, SiwxExtension, create_siwx_header,
        siwx_extension_from_payment_required,
    },
};
use tracing::{info, warn};

use crate::accounts::{AccountsStore, ResolvedEphemeral};
use crate::{Error, Result};

pub use solana_x402::{SIGN_IN_WITH_X_HEADER, X402_V1_PAYMENT_HEADER, X402_V2_PAYMENT_HEADER};

#[derive(Debug, Clone)]
pub struct Challenge {
    pub x402_version: u64,
    pub requirements: PaymentRequirements,
    pub siwx: Option<SiwxExtension>,
}

#[derive(Debug, Clone)]
pub struct SiwxAuthChallenge {
    pub extension: SiwxExtension,
}

#[derive(Debug)]
pub struct BuiltPayment {
    pub headers: Vec<(&'static str, String)>,
    pub ephemeral_notice: Option<ResolvedEphemeral>,
}

/// Try to parse an x402 challenge from headers and/or body.
/// Defaults to preferring Solana mainnet when multiple chains are offered.
pub fn parse(headers: &[(String, String)], body: Option<&str>) -> Option<Challenge> {
    let requirements = parse_x402_challenge_for_network(headers, body, Some(SOLANA_MAINNET))?;
    let siwx = parse_siwx_extension(headers, body).ok().flatten();
    Some(Challenge {
        x402_version: detect_x402_version(headers, body),
        requirements,
        siwx,
    })
}

/// Try to parse an auth-only x402 SIWX challenge.
pub fn parse_siwx_auth(
    headers: &[(String, String)],
    body: Option<&str>,
) -> Option<SiwxAuthChallenge> {
    let envelope = parse_payment_required_envelope(headers, body)?;
    if !envelope.accepts.is_empty() {
        return None;
    }
    let extension = siwx_extension_from_payment_required(&envelope)
        .ok()
        .flatten()?;
    Some(SiwxAuthChallenge { extension })
}

/// Build signed x402 retry headers.
///
/// The `ephemeral_notice` field is `Some` only when this call generated a fresh
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
) -> Result<BuiltPayment> {
    let requirements = &challenge.requirements;
    let amount = format_amount(&requirements.amount, &requirements.currency);
    let prompt_context = crate::client::prompt::payment_prompt_context(
        requirements.description.as_deref(),
        &[Some(requirements.resource.as_str()), resource_url],
    );
    let intent = crate::keystore::AuthIntent::authorize_payment_details(
        &amount,
        &prompt_context.reason,
        &prompt_context.operator,
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

    let (signer, ephemeral_notice) = crate::signer::load_signer_for_network_payment_with_intent(
        &network,
        store,
        account_override,
        &amount,
        &intent,
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

    let (payment_header_name, payment_header_value) = match challenge.x402_version {
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

    let mut headers = vec![(payment_header_name, payment_header_value)];
    if let Some((header_name, header_value)) = build_siwx_header(challenge, &signer, &network, &rt)?
    {
        headers.push((header_name, header_value));
    }

    Ok(BuiltPayment {
        headers,
        ephemeral_notice,
    })
}

/// Build a signed x402 SIWX-only retry header.
pub fn build_siwx_auth_header(
    challenge: &SiwxAuthChallenge,
    store: &dyn AccountsStore,
    network_override: Option<&str>,
    account_override: Option<&str>,
    resource_url: Option<&str>,
) -> Result<BuiltPayment> {
    let preferred_chain_id = network_override.and_then(siwx_chain_id_for_network);
    let chain = solana_x402::siwx::select_siwx_chain(
        &challenge.extension,
        &SiwxChainSelectionOptions {
            preferred_chain_id,
            supported_chain_ids: vec![],
        },
    )
    .map_err(|e| Error::Mpp(format!("Failed to select x402 sign-in challenge: {e}")))?;
    let network = network_override
        .map(str::to_string)
        .unwrap_or_else(|| normalize_network(&chain.chain_id));
    let desc = crate::client::prompt::payment_description(None, &[resource_url]);
    let reason = format!("authorize sign-in for {desc}");
    let (signer, ephemeral_notice) = crate::signer::load_signer_for_network_with_reason(
        &network,
        store,
        account_override,
        &reason,
    )?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Mpp(format!("Failed to create runtime: {e}")))?;
    let header = rt
        .block_on(create_siwx_header(&challenge.extension, &chain, &signer))
        .map_err(|e| Error::Mpp(format!("Failed to sign x402 sign-in challenge: {e}")))?;

    Ok(BuiltPayment {
        headers: vec![(SIGN_IN_WITH_X_HEADER, header)],
        ephemeral_notice,
    })
}

fn build_siwx_header(
    challenge: &Challenge,
    signer: &dyn SolanaSigner,
    network: &str,
    rt: &tokio::runtime::Runtime,
) -> Result<Option<(&'static str, String)>> {
    let Some(extension) = &challenge.siwx else {
        return Ok(None);
    };
    let preferred_chain_id = siwx_chain_id_for_network(network);
    let chain = solana_x402::siwx::select_siwx_chain(
        extension,
        &SiwxChainSelectionOptions {
            preferred_chain_id,
            supported_chain_ids: vec![],
        },
    )
    .map_err(|e| Error::Mpp(format!("Failed to select x402 sign-in challenge: {e}")))?;
    let header = rt
        .block_on(create_siwx_header(extension, &chain, signer))
        .map_err(|e| Error::Mpp(format!("Failed to sign x402 sign-in challenge: {e}")))?;

    Ok(Some((SIGN_IN_WITH_X_HEADER, header)))
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

fn siwx_chain_id_for_network(network: &str) -> Option<String> {
    match network {
        "mainnet" | "mainnet-beta" | "solana" => Some(SOLANA_MAINNET.to_string()),
        "devnet" | "localnet" | "solana-devnet" => Some(SOLANA_DEVNET.to_string()),
        "testnet" | "solana-testnet" => Some(SOLANA_TESTNET.to_string()),
        value if value.starts_with("solana:") => Some(value.to_string()),
        _ => None,
    }
}

fn parse_siwx_extension(
    headers: &[(String, String)],
    body: Option<&str>,
) -> Result<Option<SiwxExtension>> {
    let Some(envelope) = parse_payment_required_envelope(headers, body) else {
        return Ok(None);
    };
    siwx_extension_from_payment_required(&envelope)
        .map_err(|e| Error::Mpp(format!("Failed to parse x402 sign-in challenge: {e}")))
}

fn parse_payment_required_envelope(
    headers: &[(String, String)],
    body: Option<&str>,
) -> Option<PaymentRequiredEnvelope> {
    if let Some(value) = header_value(headers, PAYMENT_REQUIRED_HEADER)
        && let Some(envelope) = parse_payment_required_envelope_header(value)
    {
        return Some(envelope);
    }

    if let Some(value) = header_value(headers, X402_V1_PAYMENT_REQUIRED_HEADER)
        && let Some(envelope) = parse_payment_required_envelope_header(value)
    {
        return Some(envelope);
    }

    body.and_then(|body| serde_json::from_str::<PaymentRequiredEnvelope>(body).ok())
}

fn parse_payment_required_envelope_header(value: &str) -> Option<PaymentRequiredEnvelope> {
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value)
        .ok()?;
    serde_json::from_slice::<PaymentRequiredEnvelope>(&decoded).ok()
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
    } else if v >= 0.01 && is_cent_exact(v) {
        format!("{v:.2}")
    } else if v >= 0.01 {
        format_precise_value(v, 6)
    } else if v >= 0.001 {
        format!("{v:.3}")
    } else if v >= 0.0001 {
        format!("{v:.4}")
    } else {
        format!("{v:.6}")
    }
}

fn is_cent_exact(v: f64) -> bool {
    let rounded_to_cent = (v * 100.0).round() / 100.0;
    (v - rounded_to_cent).abs() < 0.0000005
}

fn format_precise_value(v: f64, decimals: usize) -> String {
    let mut value = format!("{v:.decimals$}");
    while value.contains('.') && value.ends_with('0') {
        value.pop();
    }
    value
}

fn detect_x402_version(headers: &[(String, String)], body: Option<&str>) -> u64 {
    // Mirror `parse_x402_challenge_for_network`'s selection order so the
    // version we report is the one whose payload the parser actually consumed.
    // If the parser took the v1 header but we reported V2 from the body, the
    // build path would emit a v2 envelope from v1-parsed requirements — same
    // struct shape today but a latent mismatch the moment the wire formats
    // diverge.
    if header_value(headers, PAYMENT_REQUIRED_HEADER).is_some() {
        return X402_VERSION_V2;
    }

    if let Some(value) = header_value(headers, X402_V1_PAYMENT_REQUIRED_HEADER) {
        return x402_version_from_json(value).unwrap_or(X402_VERSION_V1);
    }

    if let Some(version) = body.and_then(x402_version_from_json) {
        return version;
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
    use crate::accounts::{Account, AccountsFile, Keystore, MemoryAccountsStore};
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
    fn format_value_preserves_fractional_cent_fees() {
        assert_eq!(format_value(1.0015), "1.0015");
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
    fn format_amount_usdc_with_fee_fraction() {
        assert_eq!(format_amount("1001500", "USDC"), "$1.0015");
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
    fn parse_v1_header_with_v2_body_keeps_v1_version() {
        // Mixed-config server: only the v1 header is present, but the body
        // declares x402Version=2. The upstream parser consumes the v1 header,
        // so we must report V1 to keep the build path aligned with what was
        // parsed — otherwise we'd emit a v2 proof for a v1-only server.
        let v1_requirements = serde_json::json!({
            "scheme": EXACT_SCHEME,
            "network": "solana-devnet",
            "maxAmountRequired": "5000",
            "payTo": "abc123",
            "asset": "SOL",
            "resource": "/test"
        });
        let body = serde_json::json!({
            X402_VERSION_FIELD: X402_VERSION_V2,
            "accepts": []
        })
        .to_string();
        let headers = vec![(
            X402_V1_PAYMENT_REQUIRED_HEADER.to_string(),
            v1_requirements.to_string(),
        )];

        let challenge = parse(&headers, Some(&body)).unwrap();
        assert_eq!(challenge.x402_version, X402_VERSION_V1);
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
    fn parse_v2_payment_required_header_captures_siwx_extension() {
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
            "accepts": [selected],
            "extensions": {
                "sign-in-with-x": {
                    "domain": "api.example.com",
                    "uri": "https://api.example.com",
                    "version": "1",
                    "nonce": "nonce-123",
                    "issuedAt": "2026-04-27T00:00:00Z",
                    "supportedChains": [{
                        "chainId": SOLANA_MAINNET,
                        "type": "ed25519",
                        "signatureScheme": "siws"
                    }]
                }
            }
        });
        let encoded = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            payment_required.to_string().as_bytes(),
        );
        let headers = vec![(PAYMENT_REQUIRED_HEADER.to_string(), encoded)];

        let challenge = parse(&headers, None).unwrap();

        assert_eq!(
            challenge
                .siwx
                .as_ref()
                .map(|extension| extension.nonce.as_str()),
            Some("nonce-123")
        );
    }

    #[test]
    fn parse_siwx_auth_reads_auth_only_challenge() {
        let payment_required = serde_json::json!({
            X402_VERSION_FIELD: X402_VERSION_V2,
            "resource": {
                "url": "https://api.example.com/v1/test",
                "description": "API access"
            },
            "accepts": [],
            "extensions": {
                "sign-in-with-x": {
                    "domain": "api.example.com",
                    "uri": "https://api.example.com",
                    "version": "1",
                    "nonce": "nonce-123",
                    "issuedAt": "2026-04-27T00:00:00Z",
                    "supportedChains": [{
                        "chainId": SOLANA_MAINNET,
                        "type": "ed25519",
                        "signatureScheme": "siws"
                    }]
                }
            }
        });
        let encoded = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            payment_required.to_string().as_bytes(),
        );
        let headers = vec![(PAYMENT_REQUIRED_HEADER.to_string(), encoded)];

        let challenge = parse_siwx_auth(&headers, None).unwrap();

        assert_eq!(challenge.extension.nonce, "nonce-123");
        assert!(parse(&headers, None).is_none());
    }

    #[test]
    fn parse_siwx_auth_reads_auth_only_body() {
        let body = serde_json::json!({
            X402_VERSION_FIELD: X402_VERSION_V2,
            "accepts": [],
            "extensions": {
                "sign-in-with-x": {
                    "domain": "api.example.com",
                    "uri": "https://api.example.com",
                    "version": "1",
                    "nonce": "nonce-from-body",
                    "issuedAt": "2026-04-27T00:00:00Z",
                    "supportedChains": [{
                        "chainId": SOLANA_DEVNET,
                        "type": "ed25519",
                        "signatureScheme": "siws"
                    }]
                }
            }
        })
        .to_string();

        let challenge = parse_siwx_auth(&[], Some(&body)).unwrap();

        assert_eq!(challenge.extension.nonce, "nonce-from-body");
    }

    #[test]
    fn parse_siwx_auth_ignores_payment_challenges() {
        let selected = serde_json::json!({
            "scheme": EXACT_SCHEME,
            "network": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
            "amount": "10000",
            "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "payTo": "6cvgmdrsVxyiuPzqMCSBnS7fAmA5Mk2VG4BcfVhC8jdC",
            "maxTimeoutSeconds": 300
        });
        let payment_required = serde_json::json!({
            X402_VERSION_FIELD: X402_VERSION_V2,
            "accepts": [selected],
            "extensions": {
                "sign-in-with-x": {
                    "domain": "api.example.com",
                    "uri": "https://api.example.com",
                    "version": "1",
                    "nonce": "nonce-123",
                    "issuedAt": "2026-04-27T00:00:00Z",
                    "supportedChains": [{
                        "chainId": SOLANA_MAINNET,
                        "type": "ed25519",
                        "signatureScheme": "siws"
                    }]
                }
            }
        });
        let encoded = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            payment_required.to_string().as_bytes(),
        );
        let headers = vec![(PAYMENT_REQUIRED_HEADER.to_string(), encoded)];

        assert!(parse_siwx_auth(&headers, None).is_none());
        assert!(parse(&headers, None).unwrap().siwx.is_some());
    }

    #[test]
    fn parse_payment_challenge_survives_malformed_siwx_extension() {
        let selected = serde_json::json!({
            "scheme": EXACT_SCHEME,
            "network": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
            "amount": "10000",
            "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "payTo": "6cvgmdrsVxyiuPzqMCSBnS7fAmA5Mk2VG4BcfVhC8jdC",
            "maxTimeoutSeconds": 300
        });
        let payment_required = serde_json::json!({
            X402_VERSION_FIELD: X402_VERSION_V2,
            "accepts": [selected],
            "extensions": {
                "sign-in-with-x": {
                    "domain": "api.example.com",
                    "uri": "https://api.example.com",
                    "version": "1",
                    "issuedAt": "2026-04-27T00:00:00Z",
                    "supportedChains": []
                }
            }
        });
        let encoded = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            payment_required.to_string().as_bytes(),
        );
        let headers = vec![(PAYMENT_REQUIRED_HEADER.to_string(), encoded)];

        let challenge = parse(&headers, None).unwrap();

        assert_eq!(challenge.requirements.amount, "10000");
        assert!(challenge.siwx.is_none());
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
    fn siwx_chain_id_for_network_maps_pay_networks() {
        assert_eq!(
            siwx_chain_id_for_network("mainnet").as_deref(),
            Some(SOLANA_MAINNET)
        );
        assert_eq!(
            siwx_chain_id_for_network("devnet").as_deref(),
            Some(SOLANA_DEVNET)
        );
        assert_eq!(
            siwx_chain_id_for_network("localnet").as_deref(),
            Some(SOLANA_DEVNET)
        );
        assert_eq!(
            siwx_chain_id_for_network("testnet").as_deref(),
            Some(SOLANA_TESTNET)
        );
    }

    #[test]
    fn build_siwx_header_signs_selected_chain() {
        const TEST_KEYPAIR_BYTES: [u8; 64] = [
            41, 99, 180, 88, 51, 57, 48, 80, 61, 63, 219, 75, 176, 49, 116, 254, 227, 176, 196,
            204, 122, 47, 166, 133, 155, 252, 217, 0, 253, 17, 49, 143, 47, 94, 121, 167, 195, 136,
            72, 22, 157, 48, 77, 88, 63, 96, 57, 122, 181, 243, 236, 188, 241, 134, 174, 224, 100,
            246, 17, 170, 104, 17, 151, 48,
        ];
        let signer =
            solana_x402::solana_keychain::memory::MemorySigner::from_bytes(&TEST_KEYPAIR_BYTES)
                .unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let extension = solana_x402::siwx::SiwxExtension::new(
            solana_x402::siwx::SiwxExtensionInfo {
                domain: "api.example.com".to_string(),
                uri: "https://api.example.com".to_string(),
                statement: Some("Sign in to pay.".to_string()),
                version: "1".to_string(),
                nonce: "nonce-123".to_string(),
                issued_at: "2026-04-27T00:00:00Z".to_string(),
                expiration_time: None,
                not_before: None,
                request_id: None,
                resources: None,
            },
            solana_x402::siwx::default_solana_siwx_chains(),
        );
        let challenge = Challenge {
            x402_version: X402_VERSION_V2,
            requirements: sample_requirements(),
            siwx: Some(extension),
        };

        let (header_name, header_value) = build_siwx_header(&challenge, &signer, "devnet", &rt)
            .unwrap()
            .unwrap();
        let payload = solana_x402::siwx::parse_siwx_header(&header_value).unwrap();

        assert_eq!(header_name, SIGN_IN_WITH_X_HEADER);
        assert_eq!(payload.chain_id, SOLANA_DEVNET);
        assert!(solana_x402::siwx::verify_siwx_payload(&payload).unwrap());
    }

    #[test]
    fn build_siwx_auth_header_signs_without_payment() {
        const TEST_KEYPAIR_BYTES: [u8; 64] = [
            41, 99, 180, 88, 51, 57, 48, 80, 61, 63, 219, 75, 176, 49, 116, 254, 227, 176, 196,
            204, 122, 47, 166, 133, 155, 252, 217, 0, 253, 17, 49, 143, 47, 94, 121, 167, 195, 136,
            72, 22, 157, 48, 77, 88, 63, 96, 57, 122, 181, 243, 236, 188, 241, 134, 174, 224, 100,
            246, 17, 170, 104, 17, 151, 48,
        ];
        let pubkey = "4BuiY9QUUfPoAGNJBja3JapAuVWMc9c7in6UCgyC2zPR";
        let account = Account {
            keystore: Keystore::Ephemeral,
            active: true,
            auth_required: Some(false),
            pubkey: Some(pubkey.to_string()),
            vault: None,
            account: None,
            path: None,
            secret_key_b58: Some(bs58::encode(TEST_KEYPAIR_BYTES).into_string()),
            created_at: Some("2026-04-27T00:00:00Z".to_string()),
        };
        let mut file = AccountsFile::default();
        file.upsert("devnet", "default", account);
        let store = MemoryAccountsStore::with_file(file);
        let challenge = SiwxAuthChallenge {
            extension: solana_x402::siwx::SiwxExtension::new(
                solana_x402::siwx::SiwxExtensionInfo {
                    domain: "api.example.com".to_string(),
                    uri: "https://api.example.com".to_string(),
                    statement: Some("Sign in.".to_string()),
                    version: "1".to_string(),
                    nonce: "nonce-123".to_string(),
                    issued_at: "2026-04-27T00:00:00Z".to_string(),
                    expiration_time: None,
                    not_before: None,
                    request_id: None,
                    resources: None,
                },
                solana_x402::siwx::default_solana_siwx_chains(),
            ),
        };

        let built = build_siwx_auth_header(&challenge, &store, Some("devnet"), None, None).unwrap();
        let payload = solana_x402::siwx::parse_siwx_header(&built.headers[0].1).unwrap();

        assert_eq!(built.headers.len(), 1);
        assert_eq!(built.headers[0].0, SIGN_IN_WITH_X_HEADER);
        assert_eq!(payload.address, pubkey);
        assert_eq!(payload.chain_id, SOLANA_DEVNET);
        assert!(solana_x402::siwx::verify_siwx_payload(&payload).unwrap());
    }

    #[test]
    fn build_siwx_auth_header_rejects_unsupported_preferred_chain() {
        let extension = solana_x402::siwx::SiwxExtension::new(
            solana_x402::siwx::SiwxExtensionInfo {
                domain: "api.example.com".to_string(),
                uri: "https://api.example.com".to_string(),
                statement: Some("Sign in.".to_string()),
                version: "1".to_string(),
                nonce: "nonce-123".to_string(),
                issued_at: "2026-04-27T00:00:00Z".to_string(),
                expiration_time: None,
                not_before: None,
                request_id: None,
                resources: None,
            },
            vec![solana_x402::siwx::SupportedChain::solana(SOLANA_MAINNET)],
        );
        let challenge = SiwxAuthChallenge { extension };
        let store = MemoryAccountsStore::new();

        let err =
            build_siwx_auth_header(&challenge, &store, Some("devnet"), None, None).unwrap_err();

        assert!(
            err.to_string()
                .contains("siwx_preferred_chain_not_supported")
        );
        assert_eq!(store.save_count(), 0);
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
            siwx: None,
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
            siwx: None,
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
            siwx: None,
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
