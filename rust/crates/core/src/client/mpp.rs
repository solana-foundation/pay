//! MPP (Machine Payments Protocol) support.
//!
//! Thin wrapper around `solana_mpp` for challenge detection and credential building.

use solana_mpp::client::build_credential_header;
use solana_mpp::protocol::solana::default_rpc_url;
use solana_mpp::solana_keychain::SolanaSigner;
use solana_mpp::solana_rpc_client::rpc_client::RpcClient;
use solana_mpp::{ChargeRequest, parse_www_authenticate_all};
use tracing::{info, warn};

use crate::accounts::{
    AccountChoice, AccountsStore, ResolvedEphemeral, resolve_account_for_network,
};
use crate::{Error, Result};

// Re-export the challenge type for the runner/CLI.
pub use solana_mpp::PaymentChallenge as Challenge;

/// Try to extract an MPP challenge from the `www-authenticate` header value.
pub fn parse(header_value: &str) -> Option<Challenge> {
    parse_all([header_value]).into_iter().next()
}

/// Extract every MPP challenge from repeated or combined `WWW-Authenticate`
/// header values.
pub fn parse_all<'a>(header_values: impl IntoIterator<Item = &'a str>) -> Vec<Challenge> {
    parse_www_authenticate_all(header_values)
        .into_iter()
        .filter_map(|result| result.ok())
        .collect()
}

/// Extract every MPP challenge from a lowercase header list.
pub fn parse_headers(headers: &[(String, String)]) -> Vec<Challenge> {
    parse_all(
        headers
            .iter()
            .filter(|(name, _)| name == "www-authenticate")
            .map(|(_, value)| value.as_str()),
    )
}

/// Build a signed credential and return the `Authorization` header value
/// alongside an optional `ResolvedEphemeral` notice that the caller should
/// render if `Some` (signals "we just generated a fresh ephemeral wallet
/// for this network — let the user know what its pubkey is").
///
/// Network resolution:
///
/// 1. `network_override` (if `Some`) — set by `--mainnet` / `--sandbox`
///    CLI flags. Forces a specific network slug regardless of what the
///    challenge advertises.
/// 2. Otherwise, `challenge.method_details.network`.
/// 3. Otherwise, `mainnet`.
pub fn build_credential(
    challenge: &Challenge,
    store: &dyn AccountsStore,
    network_override: Option<&str>,
    account_override: Option<&str>,
    resource_url: Option<&str>,
) -> Result<(String, Option<ResolvedEphemeral>)> {
    let request: ChargeRequest = challenge
        .request
        .decode()
        .map_err(|e| Error::Mpp(format!("Failed to decode challenge request: {e}")))?;

    let amount = format_amount(&request.amount, &request.currency);
    let desc = crate::client::prompt::payment_description(
        challenge.description.as_deref(),
        &[resource_url],
    );

    let challenge_network = request
        .method_details
        .as_ref()
        .and_then(|v| v.get("network"))
        .and_then(|v| v.as_str())
        .unwrap_or("mainnet")
        .to_string();
    let challenge_network = normalize_network(&challenge_network).to_string();
    let embedded_blockhash = request
        .method_details
        .as_ref()
        .and_then(|v| v.get("recentBlockhash"))
        .and_then(|v| v.as_str());

    // Client-side network intent check: refuse to sign if the user
    // explicitly forced a network slug via `--sandbox`/`--mainnet` and
    // the server's challenge advertises a different one. Better to
    // abort here with a clear error than to sign a credential that
    // either gets rejected by the verifier or — worse — somehow
    // succeeds against the wrong cluster.
    check_client_network_intent(network_override, &challenge_network, embedded_blockhash)?;

    // Auto-funding via Surfpool runs when the user explicitly opted into
    // sandbox (`--sandbox`/`--local`) OR when the challenge embeds a
    // Surfpool blockhash — meaning we hit a sandbox gateway without a flag.
    // The `surfnet_setTokenAccount` cheatcode is required to properly
    // initialize token accounts in surfpool's local state; JIT-fetched
    // accounts from mainnet are read-only and fail simulation.
    let is_surfpool_challenge =
        embedded_blockhash.is_some_and(|h| h.starts_with(SURFPOOL_BLOCKHASH_PREFIX));
    let user_opted_into_sandbox = network_override.is_some() || is_surfpool_challenge;
    let network = network_override
        .map(str::to_string)
        .unwrap_or(challenge_network);

    let (signer, ephemeral_notice) = crate::signer::load_signer_for_network_payment(
        &network,
        store,
        account_override,
        &amount,
        &desc,
    )?;

    let rpc_url = resolve_rpc_url(&network, embedded_blockhash);
    let rpc = RpcClient::new(rpc_url.clone());

    info!(
        amount = %request.amount,
        currency = %request.currency,
        network = %network,
        %rpc_url,
        signer = %signer.pubkey(),
        "Building MPP credential"
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Mpp(format!("Failed to create runtime: {e}")))?;

    // Auto-fund when in sandbox mode. We fund on every call (idempotent)
    // because Surfpool requires `surfnet_setTokenAccount` to properly
    // initialize token accounts — JIT-fetched accounts from mainnet
    // fail simulation without it.
    if user_opted_into_sandbox {
        let pubkey = signer.pubkey().to_string();
        let fund_url = rpc_url.clone();
        if let Err(e) = rt.block_on(crate::client::sandbox::fund_via_surfpool(
            &fund_url, &pubkey,
        )) {
            warn!(error = %e, "Could not auto-fund ephemeral via Surfpool — broadcast may fail if wallet is empty");
        }
    }

    let header = rt
        .block_on(build_credential_header(&signer, &rpc, challenge))
        .map_err(|e| Error::Mpp(format!("Failed to build credential: {e}")))?;

    Ok((header, ephemeral_notice))
}

/// Select a charge challenge the configured wallet can actually pay.
///
/// If balances cannot be fetched, the first Solana charge challenge is
/// returned so older payment paths continue to work. If balances are known and
/// none of the advertised currencies is funded enough, payment is rejected
/// before the client signs anything.
pub fn select_challenge_by_balance<'a>(
    challenges: &'a [Challenge],
    store: &dyn AccountsStore,
    network_override: Option<&str>,
    account_override: Option<&str>,
) -> Result<Option<&'a Challenge>> {
    let candidates = decoded_charge_candidates(challenges, network_override)?;
    let Some(first) = candidates.first() else {
        return Ok(None);
    };

    let network = normalize_network(&first.network);
    let pubkey = match account_pubkey_for_network(store, network, account_override)? {
        Some(pubkey) => pubkey,
        None => return Ok(Some(&challenges[first.index])),
    };

    let rpc_url = resolve_rpc_url(&first.network, first.embedded_blockhash.as_deref());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Mpp(format!("Failed to create runtime: {e}")))?;

    if first.user_opted_into_sandbox
        && let Err(e) = rt.block_on(crate::client::sandbox::fund_via_surfpool(&rpc_url, &pubkey))
    {
        warn!(error = %e, "Could not auto-fund wallet via Surfpool before challenge selection");
    }

    let balances = match rt.block_on(crate::client::balance::get_balances(&rpc_url, &pubkey)) {
        Ok(balances) => balances,
        Err(e) => {
            warn!(error = %e, %rpc_url, %pubkey, "Could not fetch balances for MPP challenge selection");
            return Ok(Some(&challenges[first.index]));
        }
    };

    if let Some(index) = select_candidate_index_for_balances(&candidates, &balances) {
        let selected = &candidates[index];
        info!(
            currency = %selected.currency,
            amount = selected.amount,
            network = %selected.network,
            pubkey = %pubkey,
            "Selected MPP challenge based on wallet balance"
        );
        return Ok(Some(&challenges[selected.index]));
    }

    Err(Error::PaymentRejected(format!(
        "wallet `{pubkey}` does not have enough balance on `{network}` for any advertised MPP challenge"
    )))
}

#[derive(Debug, Clone)]
struct DecodedChargeCandidate {
    index: usize,
    amount: u64,
    currency: String,
    mint: Option<String>,
    network: String,
    embedded_blockhash: Option<String>,
    user_opted_into_sandbox: bool,
}

fn decoded_charge_candidates(
    challenges: &[Challenge],
    network_override: Option<&str>,
) -> Result<Vec<DecodedChargeCandidate>> {
    let mut candidates = Vec::new();

    for (index, challenge) in challenges.iter().enumerate() {
        if !solana_mpp::client::is_solana_charge_challenge(challenge) {
            continue;
        }

        let request: ChargeRequest = challenge
            .request
            .decode()
            .map_err(|e| Error::Mpp(format!("Failed to decode challenge request: {e}")))?;
        let details: solana_mpp::protocol::solana::MethodDetails = request
            .method_details
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| Error::Mpp(format!("Failed to decode Solana method details: {e}")))?
            .unwrap_or_default();

        let challenge_network = details
            .network
            .clone()
            .unwrap_or_else(|| "mainnet".to_string());
        let challenge_network = normalize_network(&challenge_network).to_string();
        if let Some(forced) = network_override {
            if forced != challenge_network {
                continue;
            }
            check_client_network_intent(
                Some(forced),
                &challenge_network,
                details.recent_blockhash.as_deref(),
            )?;
        }

        let network = network_override
            .map(str::to_string)
            .unwrap_or(challenge_network);
        let is_surfpool_challenge = details
            .recent_blockhash
            .as_deref()
            .is_some_and(|h| h.starts_with(SURFPOOL_BLOCKHASH_PREFIX));
        let mint = solana_mpp::protocol::solana::resolve_stablecoin_mint(
            &request.currency,
            Some(&network),
        )
        .map(str::to_string);

        candidates.push(DecodedChargeCandidate {
            index,
            amount: request
                .amount
                .parse()
                .map_err(|_| Error::Mpp(format!("Invalid challenge amount: {}", request.amount)))?,
            currency: request.currency,
            mint,
            network,
            embedded_blockhash: details.recent_blockhash,
            user_opted_into_sandbox: network_override.is_some() || is_surfpool_challenge,
        });
    }

    Ok(candidates)
}

fn select_candidate_index_for_balances(
    candidates: &[DecodedChargeCandidate],
    balances: &crate::client::balance::AccountBalances,
) -> Option<usize> {
    candidates
        .iter()
        .position(|candidate| has_sufficient_balance(candidate, balances))
}

fn has_sufficient_balance(
    candidate: &DecodedChargeCandidate,
    balances: &crate::client::balance::AccountBalances,
) -> bool {
    match &candidate.mint {
        None => balances.sol_lamports >= candidate.amount,
        Some(mint) => balances
            .tokens
            .iter()
            .any(|token| token.mint == *mint && token.raw_amount >= candidate.amount),
    }
}

fn account_pubkey_for_network(
    store: &dyn AccountsStore,
    network: &str,
    account_override: Option<&str>,
) -> Result<Option<String>> {
    let file = store.load()?;
    if let Some(name) = account_override {
        return Ok(file
            .named_account_for_network(network, name)
            .and_then(|account| account.pubkey.clone()));
    }

    match resolve_account_for_network(network, &file) {
        AccountChoice::Resolved { account, .. } => Ok(account.pubkey),
        AccountChoice::Missing => Ok(None),
    }
}

fn resolve_rpc_url(network: &str, embedded_blockhash: Option<&str>) -> String {
    std::env::var("PAY_RPC_URL").unwrap_or_else(|_| {
        if network == "localnet"
            && embedded_blockhash.is_some_and(|h| h.starts_with(SURFPOOL_BLOCKHASH_PREFIX))
        {
            crate::config::SANDBOX_RPC_URL.to_string()
        } else {
            default_rpc_url(network).to_string()
        }
    })
}

fn normalize_network(network: &str) -> &str {
    match network {
        "mainnet-beta" => "mainnet",
        other => other,
    }
}

/// Base58 prefix that the Surfpool sandbox embeds in every blockhash it
/// returns. The same constant lives in the SDK's server-side check; we
/// duplicate it here so the client doesn't pull in a server-only feature.
pub(crate) const SURFPOOL_BLOCKHASH_PREFIX: &str = "SURFNETxSAFEHASH";

/// Pure check: refuse to sign a credential when the user explicitly
/// forced a network slug (via `--sandbox`/`--mainnet`) but the server's
/// challenge advertises a different one.
///
/// Two failure modes:
///
/// 1. **Slug mismatch** — the user said `--sandbox` (forces `localnet`)
///    but the server's `methodDetails.network` says `mainnet`. The user
///    is trying to pay a real-money endpoint with a sandbox flag — abort.
///
/// 2. **Embedded-blockhash mismatch** — the user forced `localnet` AND
///    the slug agrees, but the server pre-fetched a non-Surfpool
///    blockhash and embedded it in the challenge. That means the server
///    is on a *different* localnet (real `solana-test-validator`, not
///    Surfpool). Signing against it would build a tx with a non-sandbox
///    blockhash, which contradicts the user's `--sandbox` intent.
///
/// Returns `Ok(())` if no override is set (the no-flag default behavior
/// trusts the challenge). Returns `Err(Error::PaymentRejected)` so the
/// CLI renders the result through the existing `Payment rejected by
/// verifier` notice.
pub(crate) fn check_client_network_intent(
    network_override: Option<&str>,
    challenge_network: &str,
    embedded_blockhash: Option<&str>,
) -> Result<()> {
    let Some(forced) = network_override else {
        return Ok(());
    };
    if forced != challenge_network {
        return Err(Error::PaymentRejected(format!(
            "you forced network `{forced}` but the server expects `{challenge_network}`. \
             Drop the flag, or talk to a server that's on `{forced}`."
        )));
    }
    // Even when slugs match, defend against the case where the server
    // pre-fetches a blockhash from a non-Surfpool localnet RPC. The
    // user said `--sandbox`, so the embedded blockhash must look like
    // a Surfpool blockhash.
    if forced == "localnet"
        && let Some(hash) = embedded_blockhash
        && !hash.starts_with(SURFPOOL_BLOCKHASH_PREFIX)
    {
        return Err(Error::PaymentRejected(format!(
            "--sandbox/--local expects a Surfpool localnet but the server's \
             challenge embeds blockhash `{hash}`, which does not start with \
             the Surfpool prefix `{SURFPOOL_BLOCKHASH_PREFIX}`. The server is \
             on a different localnet."
        )));
    }
    Ok(())
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
        // 1000000 = 1.0 USDC
        assert_eq!(format_amount("1000000", "USDC"), "$1.00");
    }

    #[test]
    fn format_amount_sol() {
        // 1000000000 = 1.0 SOL
        assert_eq!(format_amount("1000000000", "SOL"), "$1.00");
    }

    #[test]
    fn format_amount_zero() {
        assert_eq!(format_amount("0", "USDC"), "$0");
    }

    #[test]
    fn format_amount_invalid() {
        assert_eq!(format_amount("not_a_number", "USDC"), "$0");
    }

    #[test]
    fn format_amount_sol_small() {
        // 1000000 lamports = 0.001 SOL
        assert_eq!(format_amount("1000000", "SOL"), "$0.001");
    }

    #[test]
    fn parse_returns_none_for_invalid() {
        assert!(parse("not a valid header").is_none());
    }

    fn challenge_for_currency(currency: &str, amount: &str) -> Challenge {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let request_json = serde_json::json!({
            "amount": amount,
            "currency": currency,
            "recipient": "So11111111111111111111111111111111111111112",
            "methodDetails": { "network": "mainnet" }
        });
        let b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&request_json).unwrap());
        let header = format!(
            "Payment id=\"{currency}\", realm=\"test\", method=\"solana\", intent=\"charge\", request=\"{b64}\""
        );
        parse(&header).unwrap()
    }

    #[test]
    fn parse_all_extracts_repeated_payment_challenges() {
        let first = solana_mpp::format_www_authenticate(&challenge_for_currency("USDC", "1000000"))
            .unwrap();
        let second =
            solana_mpp::format_www_authenticate(&challenge_for_currency("USDT", "1000000"))
                .unwrap();

        let parsed = parse_all([first.as_str(), second.as_str()]);
        assert_eq!(parsed.len(), 2);
        let currencies: Vec<String> = parsed
            .into_iter()
            .map(|challenge| {
                let request: ChargeRequest = challenge.request.decode().unwrap();
                request.currency
            })
            .collect();
        assert_eq!(currencies, ["USDC", "USDT"]);
    }

    #[test]
    fn balance_selector_picks_first_funded_currency() {
        let challenges = vec![
            challenge_for_currency("USDC", "1000000"),
            challenge_for_currency("USDT", "1000000"),
        ];
        let candidates = decoded_charge_candidates(&challenges, None).unwrap();
        let balances = crate::client::balance::AccountBalances {
            sol_lamports: 0,
            tokens: vec![crate::client::balance::TokenBalance {
                mint: solana_mpp::protocol::solana::mints::USDT_MAINNET.to_string(),
                raw_amount: 2_000_000,
                ui_amount: 2.0,
                symbol: Some("USDT"),
            }],
        };

        let selected = select_candidate_index_for_balances(&candidates, &balances).unwrap();
        assert_eq!(candidates[selected].currency, "USDT");
    }

    #[test]
    fn balance_selector_returns_none_when_no_currency_is_funded() {
        let challenges = vec![
            challenge_for_currency("USDC", "1000000"),
            challenge_for_currency("USDT", "1000000"),
        ];
        let candidates = decoded_charge_candidates(&challenges, None).unwrap();
        let balances = crate::client::balance::AccountBalances {
            sol_lamports: 0,
            tokens: vec![crate::client::balance::TokenBalance {
                mint: solana_mpp::protocol::solana::mints::USDT_MAINNET.to_string(),
                raw_amount: 999_999,
                ui_amount: 0.999999,
                symbol: Some("USDT"),
            }],
        };

        assert!(select_candidate_index_for_balances(&candidates, &balances).is_none());
    }

    // ── check_client_network_intent ────────────────────────────────────────
    //
    // Pure function — covers every quadrant of (override, challenge_network,
    // embedded_blockhash) plus a few edge cases.

    fn must_err(r: Result<()>) -> String {
        match r {
            Ok(()) => panic!("expected Err, got Ok"),
            Err(Error::PaymentRejected(s)) => s,
            Err(other) => panic!("expected PaymentRejected, got {other:?}"),
        }
    }

    #[test]
    fn intent_check_passes_when_no_override() {
        // Without an explicit flag, the client trusts whatever the
        // challenge says. Both slug-mismatch and weird-blockhash
        // scenarios are accepted.
        assert!(check_client_network_intent(None, "mainnet", None).is_ok());
        assert!(check_client_network_intent(None, "localnet", Some("anything")).is_ok());
        assert!(check_client_network_intent(None, "mainnet", Some("9zrUHnA1nCByPksy")).is_ok());
    }

    #[test]
    fn intent_check_passes_when_override_matches_slug() {
        assert!(check_client_network_intent(Some("mainnet"), "mainnet", None).is_ok());
        assert!(
            check_client_network_intent(
                Some("localnet"),
                "localnet",
                Some("SURFNETxSAFEHASHxxxxxxxxxxxxxxxxxxx1892bcad")
            )
            .is_ok()
        );
        // Forced localnet with no embedded blockhash → accept (the
        // client will fetch one from its own RPC).
        assert!(check_client_network_intent(Some("localnet"), "localnet", None).is_ok());
    }

    #[test]
    fn intent_check_rejects_sandbox_against_mainnet_server() {
        // The user-reported scenario: `pay --sandbox curl ...` against
        // a server with `network: mainnet`. Must abort BEFORE signing
        // with a clear "you forced X but server expects Y" message.
        let msg = must_err(check_client_network_intent(
            Some("localnet"),
            "mainnet",
            Some("9zrUHnA1nCByPksy3aL8tQ47vqdaG2vnFs4HrxgcZj4F"),
        ));
        assert!(msg.contains("forced"), "missing forced-side: {msg}");
        assert!(msg.contains("`localnet`"), "missing forced network: {msg}");
        assert!(msg.contains("`mainnet`"), "missing server network: {msg}");
    }

    #[test]
    fn intent_check_rejects_mainnet_flag_against_sandbox_server() {
        // Reverse: --mainnet against a localnet server.
        let msg = must_err(check_client_network_intent(
            Some("mainnet"),
            "localnet",
            Some("SURFNETxSAFEHASHxxxxxxxxxxxxxxxxxxx1892bcad"),
        ));
        assert!(msg.contains("`mainnet`"));
        assert!(msg.contains("`localnet`"));
    }

    #[test]
    fn intent_check_rejects_sandbox_with_non_surfpool_blockhash() {
        // Both sides agree on `localnet` slug, but the server pre-
        // fetched a non-Surfpool blockhash. The user explicitly said
        // `--sandbox`, so the server must be on Surfpool — abort.
        let msg = must_err(check_client_network_intent(
            Some("localnet"),
            "localnet",
            Some("9zrUHnA1nCByPksy3aL8tQ47vqdaG2vnFs4HrxgcZj4F"),
        ));
        assert!(
            msg.contains("Surfpool"),
            "missing Surfpool reference: {msg}"
        );
        assert!(
            msg.contains(SURFPOOL_BLOCKHASH_PREFIX),
            "missing prefix: {msg}"
        );
    }

    #[test]
    fn intent_check_accepts_sandbox_with_surfpool_blockhash() {
        // Happy path: --sandbox + localnet challenge + Surfpool-prefixed
        // embedded blockhash. Pin the design intent.
        assert!(
            check_client_network_intent(
                Some("localnet"),
                "localnet",
                Some("SURFNETxSAFEHASHxxxxxxxxxxxxxxxxxxx1892bcad"),
            )
            .is_ok()
        );
    }

    #[test]
    fn intent_check_does_not_check_blockhash_for_non_localnet_overrides() {
        // The blockhash check only applies when forcing localnet.
        // Forcing mainnet against a mainnet server with any embedded
        // blockhash should pass.
        assert!(
            check_client_network_intent(Some("mainnet"), "mainnet", Some("anything-goes-here"))
                .is_ok()
        );
    }

    #[test]
    fn intent_check_partial_prefix_does_not_satisfy_sandbox_requirement() {
        // "SURFNETx" alone (8 chars) is NOT the full prefix.
        let msg = must_err(check_client_network_intent(
            Some("localnet"),
            "localnet",
            Some("SURFNETxNotARealHash"),
        ));
        assert!(msg.contains(SURFPOOL_BLOCKHASH_PREFIX));
    }

    // ── Surfpool detection & RPC fallback ─────────────────────────────────
    //
    // Tests for the auto-detection of sandbox challenges via the embedded
    // Surfpool blockhash prefix. Covers:
    // - `user_opted_into_sandbox` derivation
    // - RPC URL fallback to SANDBOX_RPC_URL
    // - Behavior with and without `--sandbox` flag

    fn surfpool_hash() -> &'static str {
        "SURFNETxSAFEHASHxxxxxxxxxxxxxxxxxxx18b8dc98"
    }

    fn mainnet_hash() -> &'static str {
        "9zrUHnA1nCByPksy3aL8tQ47vqdaG2vnFs4HrxgcZj4F"
    }

    /// Helper: compute `user_opted_into_sandbox` using the same logic as
    /// `build_credential`.
    fn is_sandbox(network_override: Option<&str>, embedded_blockhash: Option<&str>) -> bool {
        let is_surfpool_challenge =
            embedded_blockhash.is_some_and(|h| h.starts_with(SURFPOOL_BLOCKHASH_PREFIX));
        network_override.is_some() || is_surfpool_challenge
    }

    /// Helper: compute RPC URL using the same logic as `build_credential`.
    fn resolve_rpc(
        network: &str,
        embedded_blockhash: Option<&str>,
        pay_rpc_url: Option<&str>,
    ) -> String {
        if let Some(url) = pay_rpc_url {
            url.to_string()
        } else if network == "localnet"
            && embedded_blockhash.is_some_and(|h| h.starts_with(SURFPOOL_BLOCKHASH_PREFIX))
        {
            crate::config::SANDBOX_RPC_URL.to_string()
        } else {
            default_rpc_url(network).to_string()
        }
    }

    // ── user_opted_into_sandbox detection ──

    #[test]
    fn sandbox_detected_with_explicit_flag() {
        // --sandbox sets network_override = Some("localnet")
        assert!(is_sandbox(Some("localnet"), None));
        assert!(is_sandbox(Some("localnet"), Some(surfpool_hash())));
    }

    #[test]
    fn sandbox_detected_with_mainnet_flag() {
        // --mainnet also sets network_override
        assert!(is_sandbox(Some("mainnet"), None));
    }

    #[test]
    fn sandbox_detected_via_surfpool_blockhash_without_flag() {
        // No flag but challenge has surfpool blockhash → sandbox
        assert!(is_sandbox(None, Some(surfpool_hash())));
    }

    #[test]
    fn sandbox_not_detected_without_flag_or_surfpool() {
        // No flag, mainnet blockhash → not sandbox
        assert!(!is_sandbox(None, None));
        assert!(!is_sandbox(None, Some(mainnet_hash())));
    }

    #[test]
    fn sandbox_not_detected_with_partial_surfpool_prefix() {
        // Partial prefix doesn't count
        assert!(!is_sandbox(None, Some("SURFNETxNotTheRealPrefix")));
    }

    // ── RPC URL resolution ──

    #[test]
    fn rpc_uses_env_var_when_set() {
        let url = resolve_rpc(
            "localnet",
            Some(surfpool_hash()),
            Some("http://custom:8899"),
        );
        assert_eq!(url, "http://custom:8899");
    }

    #[test]
    fn rpc_falls_back_to_sandbox_for_surfpool_challenge() {
        let url = resolve_rpc("localnet", Some(surfpool_hash()), None);
        assert_eq!(url, crate::config::SANDBOX_RPC_URL);
    }

    #[test]
    fn rpc_falls_back_to_localhost_for_non_surfpool_localnet() {
        let url = resolve_rpc("localnet", Some(mainnet_hash()), None);
        assert_eq!(url, "http://localhost:8899");
    }

    #[test]
    fn rpc_falls_back_to_localhost_for_localnet_no_blockhash() {
        let url = resolve_rpc("localnet", None, None);
        assert_eq!(url, "http://localhost:8899");
    }

    #[test]
    fn rpc_falls_back_to_mainnet_for_mainnet_network() {
        let url = resolve_rpc("mainnet", None, None);
        assert_eq!(url, "https://api.mainnet-beta.solana.com");
    }

    #[test]
    fn rpc_falls_back_to_devnet_for_devnet_network() {
        let url = resolve_rpc("devnet", None, None);
        assert_eq!(url, "https://api.devnet.solana.com");
    }

    #[test]
    fn rpc_ignores_surfpool_blockhash_for_non_localnet() {
        // Even if blockhash looks like surfpool, non-localnet uses default
        let url = resolve_rpc("mainnet", Some(surfpool_hash()), None);
        assert_eq!(url, "https://api.mainnet-beta.solana.com");
    }
}
