//! Client-side support for the MPP `subscription` intent.
//!
//! Mirrors [`crate::client::mpp`] but specialised to `intent="subscription"`:
//! parses the 402 challenge, builds the activation transaction via
//! `solana_mpp::client::build_subscription_activation_transaction`, formats
//! the `Authorization: Payment` header, and persists the resulting
//! `Subscription` into `~/.config/pay/accounts.yml` when activation settles.
//!
//! Renewals are server-driven on-chain transactions and do not pass through
//! this module — only activation produces an HTTP credential.
//!
//! See `docs/subscriptions.md` and
//! `mpp-specs/specs/methods/solana/draft-solana-subscription-00.md` for the
//! authoritative wire shapes.

use solana_mpp::client::{
    build_subscription_activation_transaction_with_options, BuildSubscriptionActivationOptions,
    SubscriptionMethodDetails,
};
use solana_mpp::format_authorization;
use solana_mpp::protocol::core::PaymentCredential;
use solana_mpp::protocol::intents::{
    SubscriptionPeriodUnit, SubscriptionReceiptExtensions, SubscriptionRequest,
};
use solana_mpp::solana_keychain::SolanaSigner;
use solana_mpp::solana_rpc_client::rpc_client::RpcClient;
use tracing::{info, warn};

use crate::accounts::{
    AccountChoice, AccountsStore, ResolvedEphemeral, Subscription, SubscriptionStatus,
    resolve_account_for_network,
};
use crate::client::mpp::Challenge;
use crate::{Error, Result};

/// Parsed subscription challenge, useful for both the dispatcher (deciding
/// whether to surface the prompt) and the actual sign-and-retry path.
#[derive(Debug, Clone)]
pub struct DecodedSubscriptionChallenge {
    pub request: SubscriptionRequest,
    pub method_details: SubscriptionMethodDetails,
    pub network: String,
    pub period_unit: SubscriptionPeriodUnit,
    pub period_count: u64,
    /// Amount in mint base units, mirroring the spec wire form.
    pub amount_base_units: String,
    /// Decimal precision of the mint as advertised by the server.
    pub decimals: u8,
    /// Symbolic currency (e.g. "USDC") when resolvable from the mint;
    /// otherwise the raw mint b58.
    pub currency_label: String,
}

/// Outcome returned from [`build_credential`] — the formatted `Authorization`
/// header plus the context needed to persist a [`Subscription`] once the
/// activation settles.
pub struct BuiltCredential {
    /// `Authorization: Payment <base64url(credential)>` ready to set on the
    /// retry request.
    pub authorization: String,
    /// Decoded challenge state. Caller threads this back into
    /// [`persist_from_receipt`] after observing a `Payment-Receipt`.
    pub decoded: DecodedSubscriptionChallenge,
    /// Subscriber pubkey (b58) bound into the activation transaction.
    pub subscriber: String,
    /// Account name within the resolved network the activation signed under.
    pub account_name: String,
    /// Network slug used for both signing and persistence.
    pub network: String,
    /// Notice for the caller when a fresh ephemeral wallet was generated.
    pub ephemeral_notice: Option<ResolvedEphemeral>,
    /// Resource URL the activation was issued against, mirrored into the
    /// stored subscription so `pay subscriptions list` can surface it.
    pub resource_url: Option<String>,
    /// Human-readable description echoed from the challenge.
    pub description: Option<String>,
}

/// Try to extract a `subscription`-intent challenge from a `WWW-Authenticate`
/// header value. Returns `None` for non-subscription challenges so callers
/// can fall through to `mpp::parse` for charge.
pub fn parse(header_value: &str) -> Option<Challenge> {
    let challenge = crate::client::mpp::parse(header_value)?;
    if is_subscription_challenge(&challenge) {
        Some(challenge)
    } else {
        None
    }
}

/// Extract every subscription challenge from a lowercase header list. Mirrors
/// [`crate::client::mpp::parse_headers`] so the dispatch loop can ask each
/// intent module in turn.
pub fn parse_headers(headers: &[(String, String)]) -> Vec<Challenge> {
    crate::client::mpp::parse_headers(headers)
        .into_iter()
        .filter(is_subscription_challenge)
        .collect()
}

/// Returns true when a `PaymentChallenge` carries `intent="subscription"` and
/// `method="solana"`. Both are required by the spec, and the local CLI only
/// implements the Solana method profile.
pub fn is_subscription_challenge(challenge: &Challenge) -> bool {
    challenge.intent.as_str() == "subscription" && challenge.method.as_str() == "solana"
}

/// Decode a subscription challenge into a strongly-typed `DecodedSubscriptionChallenge`.
///
/// Performs all the validation that doesn't need a signer or RPC (the
/// challenge JSON, `methodDetails`, mapped period bounds) so the caller can
/// surface clear errors before prompting Touch ID.
pub fn decode(challenge: &Challenge) -> Result<DecodedSubscriptionChallenge> {
    let request: SubscriptionRequest = challenge
        .request
        .decode()
        .map_err(|e| Error::Mpp(format!("Failed to decode subscription request: {e}")))?;

    let method_details_value = request
        .method_details
        .clone()
        .ok_or_else(|| Error::Mpp("Subscription challenge is missing methodDetails".into()))?;
    let method_details = SubscriptionMethodDetails::from_json(&method_details_value)
        .map_err(|e| Error::Mpp(format!("Invalid subscription methodDetails: {e}")))?;

    // The Solana profile uses on-chain Plan PDAs; the SDK reads `planId` from
    // methodDetails. We keep validation strict so a misconfigured challenge
    // fails before the user pays.
    if method_details.plan_id.is_empty() {
        return Err(Error::Mpp(
            "Subscription challenge missing methodDetails.planId".into(),
        ));
    }

    let period_count = request
        .parse_period_count()
        .map_err(|e| Error::Mpp(e.to_string()))?;
    let _ = request
        .period_hours()
        .map_err(|e| Error::Mpp(e.to_string()))?;

    let network = method_details_value
        .get("network")
        .and_then(|v| v.as_str())
        .unwrap_or("mainnet")
        .to_string();

    let decimals = method_details_value
        .get("decimals")
        .and_then(|v| v.as_u64())
        .unwrap_or(6) as u8;

    // The challenge stores the mint b58 in `currency`. For display we prefer
    // a known stablecoin symbol; otherwise fall back to a short prefix of the
    // mint so list/status rows stay readable.
    let currency_label = pay_types::Stablecoin::from_mint(&request.currency)
        .map(|c| c.symbol().to_string())
        .unwrap_or_else(|| {
            if request.currency.len() > 8 {
                format!("{}…", &request.currency[..8])
            } else {
                request.currency.clone()
            }
        });

    Ok(DecodedSubscriptionChallenge {
        amount_base_units: request.amount.clone(),
        period_unit: request.period_unit,
        period_count,
        request,
        method_details,
        network: normalize_network(&network).to_string(),
        decimals,
        currency_label,
    })
}

/// Build a signed activation credential and return the `Authorization`
/// header value plus the context needed for post-activation persistence.
///
/// Network resolution mirrors [`crate::client::mpp::build_credential`]:
/// `network_override` wins, otherwise `methodDetails.network`, otherwise
/// `mainnet`.
pub fn build_credential(
    challenge: &Challenge,
    store: &dyn AccountsStore,
    network_override: Option<&str>,
    account_override: Option<&str>,
    resource_url: Option<&str>,
) -> Result<BuiltCredential> {
    let decoded = decode(challenge)?;

    let amount_label = format_amount(&decoded.amount_base_units, decoded.decimals);
    let period_label = format!(
        "{count} {unit}{plural}",
        count = decoded.period_count,
        unit = period_unit_name(decoded.period_unit),
        plural = if decoded.period_count == 1 { "" } else { "s" }
    );
    let reason = decoded
        .request
        .description
        .clone()
        .or_else(|| challenge.description.clone())
        .unwrap_or_else(|| {
            format!("Subscribe ({amount_label} {currency} every {period_label})",
                currency = decoded.currency_label)
        });
    let prompt_context =
        crate::client::prompt::payment_prompt_context(Some(&reason), &[resource_url]);
    let intent_reason = format!(
        "Recurring subscription — {amount_label} {currency} every {period_label}",
        currency = decoded.currency_label
    );
    let auth_intent = crate::keystore::AuthIntent::authorize_payment_details(
        &amount_label,
        &intent_reason,
        &prompt_context.operator,
    );

    // Same intent-vs-network check as charge — refuse to sign if the user
    // forced a network slug that contradicts the server.
    let embedded_blockhash = decoded.method_details.recent_blockhash.as_deref();
    crate::client::mpp::check_client_network_intent(
        network_override,
        &decoded.network,
        embedded_blockhash,
    )?;

    let network = network_override
        .map(str::to_string)
        .unwrap_or_else(|| decoded.network.clone());

    let (signer, ephemeral_notice) = crate::signer::load_signer_for_network_payment_with_intent(
        &network,
        store,
        account_override,
        &amount_label,
        &auth_intent,
    )?;
    let subscriber = signer.pubkey().to_string();

    let rpc_url = resolve_rpc_url(&network, embedded_blockhash);
    // `confirmed` (not the default `finalized`) — the interactive 402
    // flow blocks on this round-trip and finalisation costs ~13 extra
    // seconds for no UX gain. The SubscriptionAuthority init we send
    // through this client is also recovered automatically on the next
    // request if the cluster forks past it, which is vanishingly rare.
    let rpc = RpcClient::new_with_commitment(
        rpc_url.clone(),
        solana_commitment_config::CommitmentConfig::confirmed(),
    );

    info!(
        amount = %decoded.amount_base_units,
        currency = %decoded.currency_label,
        plan = %decoded.method_details.plan_id,
        network = %network,
        %rpc_url,
        signer = %subscriber,
        "Building subscription activation credential"
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Mpp(format!("Failed to create runtime: {e}")))?;

    // Surfpool sandbox: same auto-fund cheatcode as the charge path so the
    // first-period transfer's source ATA exists on-chain.
    if crate::client::mpp::should_auto_fund_surfpool(network_override, embedded_blockhash) {
        let fund_url = rpc_url.clone();
        let pubkey = subscriber.clone();
        if let Err(e) =
            rt.block_on(crate::client::sandbox::fund_via_surfpool(&fund_url, &pubkey))
        {
            warn!(error = %e, "Could not auto-fund subscriber via Surfpool — broadcast may fail");
        }
    }

    let payload = rt
        .block_on(build_subscription_activation_transaction_with_options(
            &signer,
            &rpc,
            &decoded.method_details,
            BuildSubscriptionActivationOptions {
                external_id: decoded.request.external_id.clone(),
                ..Default::default()
            },
        ))
        .map_err(|e| Error::Mpp(format!("Failed to build activation transaction: {e}")))?;

    let credential = PaymentCredential::new(challenge.to_echo(), payload);
    let authorization = format_authorization(&credential)
        .map_err(|e| Error::Mpp(format!("Failed to format subscription credential: {e}")))?;

    // Account name resolution: the override wins, else we re-read the
    // resolver the signer used. We need this for persistence so the
    // subscription row lands under the right `(network, account)` tuple.
    let account_name = resolve_account_name(store, &network, account_override)?;

    Ok(BuiltCredential {
        authorization,
        decoded,
        subscriber,
        account_name,
        network,
        ephemeral_notice,
        resource_url: resource_url.map(str::to_string),
        description: extract_description(challenge),
    })
}

/// Parse a `Payment-Receipt` header into a [`Subscription`] and persist it
/// under the account that signed the activation.
///
/// `built` is the value returned by [`build_credential`]; this function is
/// intended to be called immediately after the retry sees a 2xx response so
/// we record the freshly-activated subscription before any further work.
///
/// The standard pay-kit `Receipt` struct does not yet model subscription
/// extension fields (`subscriptionId`, `periodIndex`, `periodStartTs`,
/// `periodEndTs`, `expiresAt`). We therefore parse the base64url-encoded
/// receipt JSON directly here, extracting both the standard fields and the
/// subscription-extension fields the spec adds. A follow-up should widen
/// `solana_mpp::Receipt` to include a `metadata` map and drop this local
/// parsing.
pub fn persist_from_receipt(
    built: &BuiltCredential,
    receipt_header: &str,
    store: &dyn AccountsStore,
) -> Result<Subscription> {
    let extensions = parse_subscription_receipt(receipt_header)?;
    let subscription = subscription_from_built_and_extensions(built, &extensions);

    let mut file = store.load()?;
    file.upsert_subscription(&built.network, &built.account_name, subscription.clone())?;
    store.save(&file)?;
    info!(
        subscription_id = %subscription.subscription_id,
        plan_id = %subscription.plan_id,
        network = %built.network,
        account = %built.account_name,
        "Persisted subscription after activation"
    );
    Ok(subscription)
}

/// Subscription-flavoured receipt fields parsed from a `Payment-Receipt`
/// header. Holds both the standard fields and the extensions defined by
/// the Solana subscription profile.
#[derive(Debug, Clone)]
pub struct ParsedSubscriptionReceipt {
    pub reference: String,
    pub timestamp: Option<String>,
    pub extensions: SubscriptionReceiptExtensions,
}

/// Decode a `Payment-Receipt` header value into the subscription-shaped
/// fields. Delegates to the SDK's new `ReceiptKind`-aware parser so the
/// wire shape stays in lock-step with whatever pay-kit emits.
pub fn parse_subscription_receipt(header: &str) -> Result<ParsedSubscriptionReceipt> {
    let kind = solana_mpp::parse_receipt(header.trim())
        .map_err(|e| Error::Mpp(format!("Could not parse Payment-Receipt: {e}")))?;
    match kind {
        solana_mpp::ReceiptKind::Subscription { base, extensions } => {
            Ok(ParsedSubscriptionReceipt {
                reference: base.reference,
                timestamp: Some(base.timestamp),
                extensions,
            })
        }
        solana_mpp::ReceiptKind::Charge(_) => Err(Error::Mpp(
            "Receipt is a charge receipt, not subscription".into(),
        )),
    }
}

fn subscription_from_built_and_extensions(
    built: &BuiltCredential,
    parsed: &ParsedSubscriptionReceipt,
) -> Subscription {
    Subscription {
        subscription_id: parsed.extensions.subscription_id.clone(),
        plan_id: parsed.extensions.plan_id.clone(),
        program_id: if built.decoded.method_details.program_id.as_deref()
            == Some(solana_mpp::program::subscriptions::SUBSCRIPTIONS_PROGRAM_ID)
            || built.decoded.method_details.program_id.is_none()
        {
            None
        } else {
            built.decoded.method_details.program_id.clone()
        },
        mint: built.decoded.method_details.mint.clone(),
        currency: Some(built.decoded.currency_label.clone()),
        amount_per_period: built.decoded.amount_base_units.clone(),
        period_unit: period_unit_name(built.decoded.period_unit).to_string(),
        period_count: u32::try_from(built.decoded.period_count).unwrap_or(u32::MAX),
        recipient: built.decoded.request.recipient.clone(),
        puller: built.decoded.method_details.puller.clone(),
        network: built.network.clone(),
        status: SubscriptionStatus::Active,
        activated_at: parsed
            .timestamp
            .clone()
            .unwrap_or_else(|| parsed.extensions.period_start_ts.clone()),
        activation_signature: parsed
            .extensions
            .activation_signature
            .clone()
            .unwrap_or_default(),
        last_charged_period: parsed.extensions.period_index.parse::<u64>().ok(),
        expires_at: parsed.extensions.expires_at.clone(),
        resource_url: built.resource_url.clone(),
        description: built.description.clone(),
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn extract_description(challenge: &Challenge) -> Option<String> {
    if let Some(d) = challenge.description.as_deref() {
        if !d.is_empty() {
            return Some(d.to_string());
        }
    }
    let request: SubscriptionRequest = challenge.request.decode().ok()?;
    request.description
}

fn period_unit_name(unit: SubscriptionPeriodUnit) -> &'static str {
    match unit {
        SubscriptionPeriodUnit::Day => "day",
        SubscriptionPeriodUnit::Week => "week",
    }
}

/// Derive the deterministic `subscription_id` (the `SubscriptionDelegation`
/// PDA) from a [`BuiltCredential`] and persist a fresh `Subscription` entry
/// into `accounts.yml`. Used by every activation path that doesn't see the
/// `Payment-Receipt` header (curl/wget/httpie wrappers, the MCP curl tool)
/// — the receipt would otherwise be the authoritative source of the
/// `subscription_id`. A best-effort `getSignaturesForAddress` against the
/// freshly-created PDA backfills the activation signature; if that fails
/// (RPC blip, indexer lag) it stays empty and `pay subscriptions refresh`
/// can reconcile later.
pub fn persist_local_subscription_after_activation(
    built: &BuiltCredential,
    store: &dyn crate::accounts::AccountsStore,
) -> Result<()> {
    use solana_mpp::program::subscriptions::{
        default_program_id, find_subscription_pda, parse_pubkey, SUBSCRIPTIONS_PROGRAM_ID,
    };

    let program_id = match built.decoded.method_details.program_id.as_deref() {
        Some(p) => parse_pubkey(p, "programId")
            .map_err(|e| Error::Mpp(format!("Invalid programId: {e}")))?,
        None => default_program_id(),
    };
    let plan_pda = parse_pubkey(&built.decoded.method_details.plan_id, "planId")
        .map_err(|e| Error::Mpp(format!("Invalid planId: {e}")))?;
    let subscriber = parse_pubkey(&built.subscriber, "subscriber")
        .map_err(|e| Error::Mpp(format!("Invalid subscriber: {e}")))?;
    let (subscription_pda, _) = find_subscription_pda(&plan_pda, &subscriber, &program_id);

    let activation_signature =
        lookup_activation_signature(&built.network, &subscription_pda.to_string(), None)
            .unwrap_or_default();

    let subscription = crate::accounts::Subscription {
        subscription_id: subscription_pda.to_string(),
        plan_id: built.decoded.method_details.plan_id.clone(),
        program_id: if built.decoded.method_details.program_id.as_deref()
            == Some(SUBSCRIPTIONS_PROGRAM_ID)
            || built.decoded.method_details.program_id.is_none()
        {
            None
        } else {
            built.decoded.method_details.program_id.clone()
        },
        mint: built.decoded.method_details.mint.clone(),
        currency: Some(built.decoded.currency_label.clone()),
        amount_per_period: built.decoded.amount_base_units.clone(),
        period_unit: match built.decoded.period_unit {
            solana_mpp::SubscriptionPeriodUnit::Day => "day".to_string(),
            solana_mpp::SubscriptionPeriodUnit::Week => "week".to_string(),
        },
        period_count: u32::try_from(built.decoded.period_count).unwrap_or(u32::MAX),
        recipient: built.decoded.request.recipient.clone(),
        puller: built.decoded.method_details.puller.clone(),
        network: built.network.clone(),
        status: crate::accounts::SubscriptionStatus::Active,
        activated_at: chrono::Utc::now().to_rfc3339(),
        activation_signature,
        last_charged_period: Some(0),
        expires_at: built.decoded.request.subscription_expires.clone(),
        resource_url: built.resource_url.clone(),
        description: built.description.clone(),
    };

    let mut file = store.load()?;
    file.upsert_subscription(&built.network, &built.account_name, subscription)?;
    store.save(&file)
}

/// Best-effort lookup of the activation `Subscribe` transaction signature
/// for an on-chain `SubscriptionDelegation` PDA, walking
/// `getSignaturesForAddress` and returning the oldest entry.
///
/// `rpc_url`, when `Some`, overrides the network-derived default — useful
/// for `pay subscriptions refresh --rpc-url <…>`. Returns `None` when the
/// PDA pubkey is malformed, RPC errors out, or the signature history is
/// empty (e.g. indexer lag right after a fresh activation). Callers
/// persist an empty `activation_signature` and rely on
/// `pay subscriptions refresh` to reconcile later.
pub fn lookup_activation_signature(
    network: &str,
    subscription_id: &str,
    rpc_url: Option<&str>,
) -> Option<String> {
    let pda: solana_pubkey::Pubkey = subscription_id.parse().ok()?;
    let rpc_url = rpc_url
        .map(str::to_string)
        .unwrap_or_else(|| default_rpc_url_for_network(network));
    let rpc = RpcClient::new(rpc_url);
    let sigs = rpc.get_signatures_for_address(&pda).ok()?;
    sigs.into_iter().last().map(|s| s.signature)
}

/// Map a pay-side network slug to the RPC URL pay uses for that network.
///
/// `localnet` and `surfnet` both route to the same sandbox cluster pay
/// server proxies to, so a local subscription resolves against the same
/// chain state the server saw at activation time.
pub fn default_rpc_url_for_network(network: &str) -> String {
    match network {
        "localnet" | "surfnet" => crate::config::SANDBOX_RPC_URL.to_string(),
        other => solana_mpp::protocol::solana::default_rpc_url(other).to_string(),
    }
}

fn resolve_rpc_url(network: &str, embedded_blockhash: Option<&str>) -> String {
    std::env::var("PAY_RPC_URL").unwrap_or_else(|_| {
        if network == "localnet"
            && embedded_blockhash.is_some_and(|h| {
                h.starts_with(crate::client::mpp::SURFPOOL_BLOCKHASH_PREFIX)
            })
        {
            crate::config::SANDBOX_RPC_URL.to_string()
        } else {
            solana_mpp::protocol::solana::default_rpc_url(network).to_string()
        }
    })
}

fn normalize_network(network: &str) -> &str {
    match network {
        "mainnet-beta" => "mainnet",
        other => other,
    }
}

/// Lookup the account name that the signer loader would resolve to. This
/// keeps persistence aligned with whichever wallet actually signed.
fn resolve_account_name(
    store: &dyn AccountsStore,
    network: &str,
    account_override: Option<&str>,
) -> Result<String> {
    if let Some(name) = account_override {
        return Ok(name.to_string());
    }
    let file = store.load()?;
    match resolve_account_for_network(network, &file) {
        AccountChoice::Resolved { name, .. } => Ok(name),
        AccountChoice::Missing => Ok(crate::accounts::DEFAULT_ACCOUNT_NAME.to_string()),
    }
}

fn format_amount(base_units: &str, decimals: u8) -> String {
    let raw: u128 = base_units.parse().unwrap_or(0);
    if decimals == 0 {
        return format!("${raw}");
    }
    let divisor = 10u128.pow(decimals as u32);
    let value = raw as f64 / divisor as f64;
    if (value * 100.0).round() / 100.0 == value {
        format!("${value:.2}")
    } else {
        format!("${value:.6}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    const PLAN: &str = "8tWbqLkUJoYy7zXc5h2EvCRoaQEv2xnQjUuYhc3rzCgT";
    const MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const PULLER: &str = "5fKb5cF22cFybZB1H4hLDydFhwoQy9JzKzRWaSbMkB6h";
    const RECIPIENT: &str = "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin";
    const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

    fn subscription_challenge(network: &str) -> Challenge {
        let request = serde_json::json!({
            "amount": "10000000",
            "currency": MINT,
            "periodUnit": "day",
            "periodCount": "30",
            "recipient": RECIPIENT,
            "externalId": PLAN,
            "description": "Pro feed",
            "methodDetails": {
                "planId": PLAN,
                "mint": MINT,
                "tokenProgram": TOKEN_PROGRAM,
                "puller": PULLER,
                "decimals": 6,
                "network": network,
            },
        });
        let b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&request).unwrap());
        let header = format!(
            "Payment id=\"sub-1\", realm=\"test\", method=\"solana\", \
             intent=\"subscription\", request=\"{b64}\""
        );
        crate::client::mpp::parse(&header).unwrap()
    }

    #[test]
    fn is_subscription_challenge_detects_intent_and_method() {
        let challenge = subscription_challenge("mainnet");
        assert!(is_subscription_challenge(&challenge));

        // Same request but wrapped as a charge intent — must be rejected.
        let request = serde_json::json!({
            "amount": "10000000",
            "currency": MINT,
            "recipient": RECIPIENT,
            "methodDetails": {"network": "mainnet"},
        });
        let b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&request).unwrap());
        let header = format!(
            "Payment id=\"c\", realm=\"r\", method=\"solana\", intent=\"charge\", request=\"{b64}\""
        );
        let charge = crate::client::mpp::parse(&header).unwrap();
        assert!(!is_subscription_challenge(&charge));
    }

    #[test]
    fn parse_returns_none_for_non_subscription() {
        // Build a charge header and confirm parse() rejects it.
        let request = serde_json::json!({
            "amount": "1",
            "currency": "USDC",
            "recipient": RECIPIENT,
            "methodDetails": {"network": "mainnet"},
        });
        let b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&request).unwrap());
        let header = format!(
            "Payment id=\"x\", realm=\"r\", method=\"solana\", intent=\"charge\", request=\"{b64}\""
        );
        assert!(parse(&header).is_none());
    }

    #[test]
    fn decode_extracts_period_and_plan_and_currency_symbol() {
        let challenge = subscription_challenge("mainnet");
        let decoded = decode(&challenge).expect("decode");
        assert_eq!(decoded.amount_base_units, "10000000");
        assert_eq!(decoded.period_unit, SubscriptionPeriodUnit::Day);
        assert_eq!(decoded.period_count, 30);
        assert_eq!(decoded.method_details.plan_id, PLAN);
        assert_eq!(decoded.network, "mainnet");
        // USDC mainnet mint resolves to the symbol.
        assert_eq!(decoded.currency_label, "USDC");
        assert_eq!(decoded.decimals, 6);
    }

    #[test]
    fn decode_rejects_challenge_without_method_details() {
        let request = serde_json::json!({
            "amount": "10000000",
            "currency": MINT,
            "periodUnit": "day",
            "periodCount": "30",
            "recipient": RECIPIENT,
        });
        let b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&request).unwrap());
        let header = format!(
            "Payment id=\"s\", realm=\"r\", method=\"solana\", \
             intent=\"subscription\", request=\"{b64}\""
        );
        let challenge = crate::client::mpp::parse(&header).unwrap();
        let err = decode(&challenge).unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("methoddetails"));
    }

    #[test]
    fn decode_rejects_month_period() {
        // periodUnit=month is rejected at the deserialize layer per the spec.
        let request = serde_json::json!({
            "amount": "1",
            "currency": MINT,
            "periodUnit": "month",
            "periodCount": "1",
            "recipient": RECIPIENT,
            "methodDetails": {"planId": PLAN, "mint": MINT, "tokenProgram": TOKEN_PROGRAM, "puller": PULLER},
        });
        let b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&request).unwrap());
        let header = format!(
            "Payment id=\"s\", realm=\"r\", method=\"solana\", \
             intent=\"subscription\", request=\"{b64}\""
        );
        let challenge = crate::client::mpp::parse(&header).unwrap();
        let err = decode(&challenge).unwrap_err();
        let msg = format!("{err}").to_lowercase();
        assert!(msg.contains("month") || msg.contains("period") || msg.contains("unknown"));
    }

    #[test]
    fn decode_falls_back_to_truncated_mint_when_currency_unknown() {
        let request = serde_json::json!({
            "amount": "1",
            "currency": "Bonk1111111111111111111111111111111111111111",
            "periodUnit": "day",
            "periodCount": "30",
            "recipient": RECIPIENT,
            "methodDetails": {
                "planId": PLAN,
                "mint": "Bonk1111111111111111111111111111111111111111",
                "tokenProgram": TOKEN_PROGRAM,
                "puller": PULLER,
                "decimals": 5,
                "network": "mainnet",
            },
        });
        let b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&request).unwrap());
        let header = format!(
            "Payment id=\"s\", realm=\"r\", method=\"solana\", \
             intent=\"subscription\", request=\"{b64}\""
        );
        let challenge = crate::client::mpp::parse(&header).unwrap();
        let decoded = decode(&challenge).unwrap();
        assert!(decoded.currency_label.contains("…"));
        assert_eq!(decoded.decimals, 5);
    }

    #[test]
    fn format_amount_renders_two_decimal_when_exact() {
        assert_eq!(format_amount("10000000", 6), "$10.00");
        assert_eq!(format_amount("99900000", 6), "$99.90");
        assert_eq!(format_amount("0", 6), "$0.00");
    }

    #[test]
    fn format_amount_handles_zero_decimals_and_large_values() {
        assert_eq!(format_amount("42", 0), "$42");
        assert_eq!(format_amount("123456789", 6), "$123.456789");
    }

    #[test]
    fn normalize_network_collapses_mainnet_beta() {
        assert_eq!(normalize_network("mainnet-beta"), "mainnet");
        assert_eq!(normalize_network("devnet"), "devnet");
    }

    #[test]
    fn parse_subscription_receipt_round_trip() {
        let payload = serde_json::json!({
            "method": "solana",
            "status": "success",
            "timestamp": "2026-01-15T12:03:10Z",
            "reference": "5J8signature",
            "subscriptionId": "BXQGmO5VwTrl5RfFr6Y8XQZ4nPj9QqMOiKkRn3pZ4ZE",
            "planId": PLAN,
            "periodIndex": "0",
            "periodStartTs": "2026-01-15T12:03:10Z",
            "periodEndTs": "2026-02-14T12:03:10Z",
            "expiresAt": "2026-07-14T12:00:00Z",
        });
        let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        let parsed = parse_subscription_receipt(&header).expect("parse");
        assert_eq!(parsed.reference, "5J8signature");
        assert_eq!(parsed.timestamp.as_deref(), Some("2026-01-15T12:03:10Z"));
        assert_eq!(parsed.extensions.subscription_id, "BXQGmO5VwTrl5RfFr6Y8XQZ4nPj9QqMOiKkRn3pZ4ZE");
        assert_eq!(parsed.extensions.plan_id, PLAN);
        assert_eq!(parsed.extensions.period_index, "0");
        assert_eq!(
            parsed.extensions.expires_at.as_deref(),
            Some("2026-07-14T12:00:00Z")
        );
    }

    #[test]
    fn parse_subscription_receipt_errors_on_invalid_base64() {
        let err = parse_subscription_receipt("not!valid!base64!!!").unwrap_err();
        let msg = format!("{err}").to_lowercase();
        assert!(
            msg.contains("base64url") || msg.contains("decode") || msg.contains("invalid"),
            "{err}"
        );
    }

    #[test]
    fn parse_subscription_receipt_errors_when_subscription_fields_missing() {
        // Standard receipt fields only — no subscriptionId etc.
        let payload = serde_json::json!({
            "method": "solana",
            "status": "success",
            "timestamp": "2026-01-15T12:03:10Z",
            "reference": "5J8signature",
            "challengeId": "c-1",
        });
        let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        assert!(parse_subscription_receipt(&header).is_err());
    }

    #[test]
    fn parse_headers_filters_to_subscription_only() {
        let sub = solana_mpp::format_www_authenticate(&subscription_challenge("mainnet")).unwrap();
        let charge_request = serde_json::json!({
            "amount": "1",
            "currency": "USDC",
            "recipient": RECIPIENT,
            "methodDetails": {"network": "mainnet"},
        });
        let b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&charge_request).unwrap());
        let charge_header = format!(
            "Payment id=\"c\", realm=\"r\", method=\"solana\", intent=\"charge\", request=\"{b64}\""
        );

        let headers = vec![
            ("www-authenticate".to_string(), sub),
            ("www-authenticate".to_string(), charge_header),
        ];
        let subs = parse_headers(&headers);
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].intent.as_str(), "subscription");
    }
}
