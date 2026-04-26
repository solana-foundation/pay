//! MPP (Machine Payments Protocol) support.
//!
//! Thin wrapper around `solana_mpp` for challenge detection and credential building.

use solana_mpp::client::build_credential_header;
use solana_mpp::protocol::solana::default_rpc_url;
use solana_mpp::solana_keychain::SolanaSigner;
use solana_mpp::solana_rpc_client::rpc_client::RpcClient;
use solana_mpp::{ChargeRequest, parse_www_authenticate};
use tracing::{info, warn};

use crate::accounts::{AccountsStore, ResolvedEphemeral};
use crate::{Error, Result};

// Re-export the challenge type for the runner/CLI.
pub use solana_mpp::PaymentChallenge as Challenge;

/// Try to extract an MPP challenge from the `www-authenticate` header value.
pub fn parse(header_value: &str) -> Option<Challenge> {
    parse_www_authenticate(header_value).ok()
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

    let rpc_url = std::env::var("PAY_RPC_URL").unwrap_or_else(|_| {
        if network == "localnet"
            && embedded_blockhash.is_some_and(|h| h.starts_with(SURFPOOL_BLOCKHASH_PREFIX))
        {
            crate::config::SANDBOX_RPC_URL.to_string()
        } else {
            default_rpc_url(&network).to_string()
        }
    });
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
