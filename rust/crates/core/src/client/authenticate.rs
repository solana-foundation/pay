//! Pay-side helpers for cached subscription bearer credentials.
//!
//! Current clients retain the signed `type="proof"` credential from a
//! successful subscription activation and reuse it directly. The legacy
//! SIWMPP helpers remain here so existing account files and older servers keep
//! working during migration. This module lets pay clients:
//!
//! - extract a legacy authenticate challenge from a multi-WWW-Authenticate
//!   header set ([`pick_authenticate_challenge`]),
//! - sign a legacy credential against it and persist the resulting
//!   `Authorization: Payment …` header for re-use across requests
//!   ([`sign_and_persist`]),
//! - look up a previously-cached header for a given resource URL, issued
//!   to the selected account ([`cached_header_for_account`]).
//!
//! The runner / CLI surfaces that actually attach the header on
//! outgoing requests are wired separately; this module is the
//! self-contained protocol layer.

use pay_kit::mpp::solana_keychain::TransactionSigner;
use std::sync::Arc;

use pay_kit::mpp::{
    PaymentChallenge, parse_www_authenticate,
    program::subscriptions::{default_program_id, find_subscription_pda, parse_pubkey},
};

use crate::accounts::{AccountsStore, Subscription, SubscriptionStatus};
use crate::{Error, Result};

/// Pick the `authenticate`-intent challenge out of a 402 header set.
///
/// `www_auth_headers` is the raw set of `WWW-Authenticate` header
/// values returned by the server (RFC 7235 allows multiple).
/// Returns `Some(challenge)` for the first parseable challenge whose
/// `intent == "authenticate"`, `None` otherwise. Malformed entries
/// are skipped silently — the subscription challenge in the same
/// response is what the caller falls back to.
pub fn pick_authenticate_challenge<I, S>(www_auth_headers: I) -> Option<PaymentChallenge>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    www_auth_headers
        .into_iter()
        .filter_map(|h| parse_www_authenticate(h.as_ref()).ok())
        .find(|c| c.intent.as_str() == "authenticate")
}

/// Sign an authenticate credential against the given challenge and
/// persist it on the matching subscription record in `accounts.yml`.
///
/// `subscriber_pubkey` is the signing wallet's base58 pubkey; we
/// derive the SubscriptionDelegation PDA from `(plan_id, subscriber)`
/// and pass it through to the SDK's credential builder. The token's
/// expiration is read back from the challenge so the cached row's
/// `authenticate_expires_at` matches what the server will accept.
///
/// On success returns the `Authorization: Payment …` header value
/// the caller MAY attach to the immediate retry. The same value is
/// also written to `accounts.yml` for subsequent requests.
#[allow(clippy::too_many_arguments)]
pub async fn sign_and_persist(
    store: &dyn AccountsStore,
    network: &str,
    account_name: &str,
    subscription_id: &str,
    plan_id: &str,
    program_id_override: Option<&str>,
    challenge: &PaymentChallenge,
    signer: Arc<dyn TransactionSigner>,
) -> Result<String> {
    let plan_pubkey = parse_pubkey(plan_id, "plan_id")
        .map_err(|e| Error::Mpp(format!("Invalid plan_id: {e}")))?;
    let program_pubkey = match program_id_override {
        Some(p) => parse_pubkey(p, "program_id")
            .map_err(|e| Error::Mpp(format!("Invalid program_id: {e}")))?,
        None => default_program_id(),
    };
    let subscriber_pubkey = signer.pubkey();
    let (derived_pda, _) = find_subscription_pda(&plan_pubkey, &subscriber_pubkey, &program_pubkey);
    if derived_pda.to_string() != subscription_id {
        return Err(Error::Mpp(format!(
            "subscription_id `{subscription_id}` doesn't match the PDA derived from \
             plan_id + signer pubkey (`{derived_pda}`). The cached subscription is \
             bound to a different wallet."
        )));
    }

    let header = pay_kit::mpp::client::build_authenticate_credential_header(
        signer.as_ref(),
        challenge,
        subscription_id,
    )
    .await
    .map_err(|e| Error::Mpp(format!("Failed to build authenticate credential: {e}")))?;

    // Read the server-set expiration time straight off the challenge
    // request payload so the cache TTL matches the server's
    // `period_end` exactly.
    let request: pay_kit::mpp::AuthenticateRequest = challenge
        .request
        .decode()
        .map_err(|e| Error::Mpp(format!("Decoding authenticate request: {e}")))?;
    let expires_at = request.expiration_time;

    persist_token(
        store,
        network,
        account_name,
        subscription_id,
        &header,
        &expires_at,
    )?;
    Ok(header)
}

/// Write the cached token + expiry to the subscription's row in
/// `accounts.yml`. Idempotent; later calls overwrite earlier ones.
pub fn persist_token(
    store: &dyn AccountsStore,
    network: &str,
    account_name: &str,
    subscription_id: &str,
    token: &str,
    expires_at: &str,
) -> Result<()> {
    let mut file = store.load()?;
    let Some(sub) = file
        .accounts
        .get(network)
        .and_then(|m| m.get(account_name))
        .and_then(|a| a.subscriptions.get(subscription_id))
        .cloned()
    else {
        return Err(Error::Config(format!(
            "no subscription `{subscription_id}` on {network}/{account_name} to attach \
             an authenticate token to"
        )));
    };
    let mut updated = sub;
    updated.authenticate_token = Some(token.to_string());
    updated.authenticate_expires_at = Some(expires_at.to_string());
    file.upsert_subscription(network, account_name, updated)?;
    store.save(&file)
}

/// Look up a cached `Authorization: Payment …` header for the given
/// resource URL, issued to the account that would pay for it. Supports both
/// the canonical subscription proof and legacy SIWMPP credentials stored by
/// older clients.
///
/// The bearer is the subscriber's identity at the merchant, so it is only
/// ever taken from the account the request is made as: `account_override`,
/// else each network's default account, restricted to `network_override`
/// when one is given. The subscription must also be recorded for that
/// network and its delegation must derive from that account's pubkey, so a
/// record copied under another account cannot speak for it. When the
/// selected account has no usable token, the caller goes through the normal
/// challenge instead of borrowing another account's subscription.
///
/// The URL match is prefix-based: a stored subscription's
/// `resource_url` matches any request URL that starts with it. This
/// covers both exact matches (`https://api.example.com/v1`) and
/// sub-paths (`https://api.example.com/v1/resource`).
pub fn cached_header_for_account(
    store: &dyn AccountsStore,
    network_override: Option<&str>,
    account_override: Option<&str>,
    resource_url: &str,
) -> Option<String> {
    let file = store.load().ok()?;
    let now = chrono::Utc::now();
    for (network, accounts) in &file.accounts {
        if network_override.is_some_and(|selected| selected != network) {
            continue;
        }
        let name = match account_override {
            Some(name) => name,
            None => match file.account_for_network(network) {
                Some((name, _)) => name,
                None => continue,
            },
        };
        let Some(account) = accounts.get(name) else {
            continue;
        };
        let Some(subscriber) = account
            .pubkey
            .as_deref()
            .and_then(|pubkey| parse_pubkey(pubkey, "pubkey").ok())
        else {
            continue;
        };
        for sub in account.subscriptions.values() {
            if sub.network != *network
                || !delegation_belongs_to(sub, &subscriber)
                || !is_token_usable_for(sub, resource_url, now)
            {
                continue;
            }
            if let Some(token) = sub.authenticate_token.as_deref() {
                return Some(token.to_string());
            }
        }
    }
    None
}

/// Whether `sub`'s delegation PDA is the one `subscriber` holds for its plan.
fn delegation_belongs_to(sub: &Subscription, subscriber: &solana_pubkey::Pubkey) -> bool {
    let Ok(plan) = parse_pubkey(&sub.plan_id, "plan_id") else {
        return false;
    };
    let program = match sub.program_id.as_deref() {
        Some(program) => match parse_pubkey(program, "program_id") {
            Ok(program) => program,
            Err(_) => return false,
        },
        None => default_program_id(),
    };
    let (expected, _) = find_subscription_pda(&plan, subscriber, &program);
    expected.to_string() == sub.delegation_address()
}

/// Decide whether `request_url` is the stored resource or a path
/// beneath it, with a hard boundary at the end of the stored URL.
///
/// Raw `starts_with` would treat `https://api.example.com.attacker.com/`
/// as a sub-resource of `https://api.example.com`, which would leak the
/// SIWMPP token to an attacker-controlled host. We require the byte
/// immediately following the stored URL to be a path/query/fragment
/// boundary (or end-of-string) so the host stays bound to what the
/// activation actually paid for.
fn url_is_sub_resource(request_url: &str, stored_url: &str) -> bool {
    let Some(suffix) = request_url.strip_prefix(stored_url) else {
        return false;
    };
    // Stored URL already ends with a boundary character → safe.
    if let Some(last) = stored_url.chars().last()
        && matches!(last, '/' | '?' | '#')
    {
        return true;
    }
    // Otherwise the FIRST char of the request URL past the stored
    // prefix must itself be a boundary, or there's no suffix at all
    // (exact match).
    match suffix.chars().next() {
        None => true,
        Some('/' | '?' | '#') => true,
        Some(_) => false,
    }
}

fn is_token_usable_for(
    sub: &Subscription,
    resource_url: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if !matches!(sub.status, SubscriptionStatus::Active) {
        return false;
    }
    let Some(token) = sub.authenticate_token.as_deref() else {
        return false;
    };
    if token.is_empty() {
        return false;
    }
    let Some(stored_url) = sub.resource_url.as_deref() else {
        return false;
    };
    if !url_is_sub_resource(resource_url, stored_url) {
        return false;
    }
    // Both the token and the subscription must still be live.
    [
        sub.authenticate_expires_at.as_deref(),
        sub.expires_at.as_deref(),
    ]
    .into_iter()
    .flatten()
    .all(
        |expires_at| match chrono::DateTime::parse_from_rfc3339(expires_at) {
            Ok(t) => t.with_timezone(&chrono::Utc) > now,
            Err(_) => false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{Account, AccountsFile, BackendKind, MemoryAccountsStore};
    use std::collections::BTreeMap;

    const PLAN: &str = "Amp9FrnEX17tVeZ7QnHX1Hh4TynhH4sXLRSde797vdKR";
    const SUBSCRIBER: &str = "12YtVRbxyhBVceYVtALMeSyro5jTLEsqHgm78K721WH3";
    const OTHER_SUBSCRIBER: &str = "6ayEJCQB7gwzwdbWLi65DR9RTRTrS3QunK6j9h2WjQjW";

    fn delegation_for(subscriber: &str) -> String {
        let plan = parse_pubkey(PLAN, "plan").unwrap();
        let subscriber = parse_pubkey(subscriber, "subscriber").unwrap();
        find_subscription_pda(&plan, &subscriber, &default_program_id())
            .0
            .to_string()
    }

    fn make_sub(
        id: &str,
        resource_url: Option<&str>,
        token: Option<&str>,
        expires_at: Option<&str>,
        status: SubscriptionStatus,
    ) -> Subscription {
        Subscription {
            subscription_id: id.to_string(),
            subscription_delegation: Some(delegation_for(SUBSCRIBER)),
            plan_id: PLAN.to_string(),
            program_id: None,
            mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
            currency: Some("USDC".to_string()),
            amount_per_period: "9990000".to_string(),
            period_unit: "day".to_string(),
            period_count: 30,
            recipient: "6ayEJCQB7gwzwdbWLi65DR9RTRTrS3QunK6j9h2WjQjW".to_string(),
            puller: "6ayEJCQB7gwzwdbWLi65DR9RTRTrS3QunK6j9h2WjQjW".to_string(),
            network: "mainnet".to_string(),
            status,
            activated_at: "2026-06-01T00:00:00Z".to_string(),
            activation_signature: String::new(),
            last_charged_period: Some(0),
            expires_at: None,
            resource_url: resource_url.map(str::to_string),
            description: None,
            authenticate_token: token.map(str::to_string),
            authenticate_expires_at: expires_at.map(str::to_string),
        }
    }

    fn store_with(sub: Subscription) -> MemoryAccountsStore {
        let mut subscriptions = BTreeMap::new();
        subscriptions.insert(sub.subscription_id.clone(), sub);
        let mut accounts_map = BTreeMap::new();
        accounts_map.insert(
            "default".to_string(),
            Account {
                backend: BackendKind::Ephemeral,
                provider: None,
                active: false,
                auth_required: Some(false),
                pubkey: Some(SUBSCRIBER.to_string()),
                vault: None,
                account: None,
                path: None,
                secret_key_b58: None,
                created_at: None,
                subscriptions,
            },
        );
        let mut accounts = BTreeMap::new();
        accounts.insert("mainnet".to_string(), accounts_map);
        let file = AccountsFile {
            version: 2,
            accounts,
        };
        MemoryAccountsStore::with_file(file)
    }

    #[test]
    fn cached_header_returns_token_when_url_prefix_matches_and_not_expired() {
        let token = "Payment eyJfYWtlfQ==";
        let expires = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        let sub = make_sub(
            "DerivedPda",
            Some("https://api.example.com/v1"),
            Some(token),
            Some(&expires),
            SubscriptionStatus::Active,
        );
        let store = store_with(sub);
        let hit =
            cached_header_for_account(&store, None, None, "https://api.example.com/v1/resource");
        assert_eq!(hit.as_deref(), Some(token));
    }

    #[test]
    fn cached_token_is_dropped_once_the_subscription_expires_first() {
        let token = "Payment eyJfYWtlfQ==";
        let token_expiry = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        let mut sub = make_sub(
            "DerivedPda",
            Some("https://api.example.com/v1"),
            Some(token),
            Some(&token_expiry),
            SubscriptionStatus::Active,
        );
        sub.expires_at = Some((chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339());
        let lookup = |sub: Subscription| {
            cached_header_for_account(
                &store_with(sub),
                None,
                None,
                "https://api.example.com/v1/resource",
            )
        };
        assert_eq!(lookup(sub.clone()), None);

        sub.expires_at = Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339());
        assert_eq!(lookup(sub.clone()).as_deref(), Some(token));
        // The later of the two never extends the earlier one.
        sub.authenticate_expires_at =
            Some((chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339());
        assert_eq!(lookup(sub), None);
    }

    #[test]
    fn cached_subscription_proof_without_legacy_expiry_is_usable() {
        let token = "Payment canonical-subscription-proof";
        let sub = make_sub(
            "opaque-subscription-id",
            Some("https://api.example.com/v1"),
            Some(token),
            None,
            SubscriptionStatus::Active,
        );
        let store = store_with(sub);

        assert_eq!(
            cached_header_for_account(&store, None, None, "https://api.example.com/v1/resource")
                .as_deref(),
            Some(token)
        );
    }

    #[test]
    fn cached_header_returns_none_when_token_expired() {
        let expired = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let sub = make_sub(
            "DerivedPda",
            Some("https://api.example.com/v1"),
            Some("Payment expired"),
            Some(&expired),
            SubscriptionStatus::Active,
        );
        let store = store_with(sub);
        assert!(
            cached_header_for_account(&store, None, None, "https://api.example.com/v1").is_none()
        );
    }

    #[test]
    fn cached_header_returns_none_when_status_not_active() {
        let expires = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        let sub = make_sub(
            "DerivedPda",
            Some("https://api.example.com/v1"),
            Some("Payment ok"),
            Some(&expires),
            SubscriptionStatus::Cancelled,
        );
        let store = store_with(sub);
        assert!(
            cached_header_for_account(&store, None, None, "https://api.example.com/v1").is_none()
        );
    }

    #[test]
    fn cached_header_returns_none_when_url_does_not_match() {
        let expires = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        let sub = make_sub(
            "DerivedPda",
            Some("https://api.example.com/v1"),
            Some("Payment ok"),
            Some(&expires),
            SubscriptionStatus::Active,
        );
        let store = store_with(sub);
        assert!(
            cached_header_for_account(&store, None, None, "https://other.example.com/v1").is_none()
        );
    }

    #[test]
    fn cached_header_rejects_hostname_suffix_attack() {
        // PR #374 security review: raw `starts_with` matches
        // `https://api.example.com.attacker.com/x` against stored
        // `https://api.example.com`. The boundary fix MUST reject
        // these even though the byte prefix matches.
        let expires = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        let sub = make_sub(
            "DerivedPda",
            Some("https://api.example.com"),
            Some("Payment ok"),
            Some(&expires),
            SubscriptionStatus::Active,
        );
        let store = store_with(sub);
        assert!(
            cached_header_for_account(&store, None, None, "https://api.example.com.attacker.com/x")
                .is_none(),
            "hostname-suffix attack must be rejected"
        );
        // But a real sub-resource still matches.
        assert!(
            cached_header_for_account(&store, None, None, "https://api.example.com/v1/resource")
                .is_some()
        );
        // Exact match also OK.
        assert!(cached_header_for_account(&store, None, None, "https://api.example.com").is_some());
        // Query / fragment past the host also OK.
        assert!(
            cached_header_for_account(&store, None, None, "https://api.example.com?a=1").is_some()
        );
        assert!(
            cached_header_for_account(&store, None, None, "https://api.example.com#frag").is_some()
        );
    }

    #[test]
    fn cached_header_returns_none_when_no_token_stored() {
        let sub = make_sub(
            "DerivedPda",
            Some("https://api.example.com/v1"),
            None,
            None,
            SubscriptionStatus::Active,
        );
        let store = store_with(sub);
        assert!(
            cached_header_for_account(&store, None, None, "https://api.example.com/v1").is_none()
        );
    }

    fn account(pubkey: &str, active: bool, subscriptions: Vec<Subscription>) -> Account {
        Account {
            backend: BackendKind::Ephemeral,
            provider: None,
            active,
            auth_required: Some(false),
            pubkey: Some(pubkey.to_string()),
            vault: None,
            account: None,
            path: None,
            secret_key_b58: None,
            created_at: None,
            subscriptions: subscriptions
                .into_iter()
                .map(|sub| (sub.subscription_id.clone(), sub))
                .collect(),
        }
    }

    fn live_sub(token: &str) -> Subscription {
        make_sub(
            "sub-a",
            Some("https://api.example.com/v1"),
            Some(token),
            None,
            SubscriptionStatus::Active,
        )
    }

    /// Account `alice` (mainnet default) holds a live subscription; `bob`,
    /// on the same network, does not.
    fn two_account_store() -> MemoryAccountsStore {
        let mut file = AccountsFile::default();
        file.upsert(
            "mainnet",
            "alice",
            account(SUBSCRIBER, true, vec![live_sub("Payment alice")]),
        );
        file.upsert("mainnet", "bob", account(OTHER_SUBSCRIBER, false, vec![]));
        MemoryAccountsStore::with_file(file)
    }

    #[test]
    fn cached_header_belongs_to_the_selected_account() {
        let store = two_account_store();
        let url = "https://api.example.com/v1/data";
        // Positive controls: the default account and naming it explicitly.
        assert_eq!(
            cached_header_for_account(&store, None, None, url).as_deref(),
            Some("Payment alice")
        );
        assert_eq!(
            cached_header_for_account(&store, Some("mainnet"), Some("alice"), url).as_deref(),
            Some("Payment alice")
        );
        // Bob is selected: alice's bearer must not speak for him.
        assert_eq!(
            cached_header_for_account(&store, None, Some("bob"), url),
            None
        );
        // Another network was selected: mainnet records do not apply.
        assert_eq!(
            cached_header_for_account(&store, Some("devnet"), Some("alice"), url),
            None
        );
    }

    #[test]
    fn cached_header_skips_a_non_default_account() {
        let mut file = AccountsFile::default();
        file.upsert(
            "mainnet",
            "alice",
            account(SUBSCRIBER, false, vec![live_sub("Payment alice")]),
        );
        file.upsert("mainnet", "bob", account(OTHER_SUBSCRIBER, true, vec![]));
        let store = MemoryAccountsStore::with_file(file);
        assert_eq!(
            cached_header_for_account(&store, None, None, "https://api.example.com/v1"),
            None
        );
    }

    #[test]
    fn cached_header_requires_the_delegation_and_network_of_the_account() {
        let url = "https://api.example.com/v1";
        let lookup = |sub: Subscription| {
            let mut file = AccountsFile::default();
            file.upsert("mainnet", "alice", account(SUBSCRIBER, true, vec![sub]));
            cached_header_for_account(&MemoryAccountsStore::with_file(file), None, None, url)
        };
        assert!(lookup(live_sub("Payment ok")).is_some());

        // A record whose delegation belongs to another subscriber.
        let mut foreign = live_sub("Payment foreign");
        foreign.subscription_delegation = Some(delegation_for(OTHER_SUBSCRIBER));
        assert_eq!(lookup(foreign), None);

        // A record for another network filed under this one.
        let mut misfiled = live_sub("Payment devnet");
        misfiled.network = "devnet".to_string();
        assert_eq!(lookup(misfiled), None);
    }

    #[test]
    fn pick_authenticate_challenge_skips_subscription_challenge() {
        // Build two challenges via the SDK; pick should grab the
        // authenticate one regardless of header order.
        let sub_challenge = pay_kit::mpp::PaymentChallenge::with_challenge_binding_secret(
            "test-secret",
            "test-realm",
            "solana",
            "subscription",
            pay_kit::mpp::Base64UrlJson::from_typed(&pay_kit::mpp::SubscriptionRequest {
                amount: "1".into(),
                currency: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
                period_unit: pay_kit::mpp::SubscriptionPeriodUnit::Day,
                period_count: "30".into(),
                recipient: "6ayEJCQB7gwzwdbWLi65DR9RTRTrS3QunK6j9h2WjQjW".into(),
                ..Default::default()
            })
            .unwrap(),
        );
        let auth_challenge = pay_kit::mpp::PaymentChallenge::with_challenge_binding_secret(
            "test-secret",
            "test-realm",
            "solana",
            "authenticate",
            pay_kit::mpp::Base64UrlJson::from_typed(&pay_kit::mpp::AuthenticateRequest {
                domain: "api.example.com".into(),
                uri: "https://api.example.com/v1".into(),
                version: pay_kit::mpp::SIWMPP_VERSION.into(),
                nonce: "abc".into(),
                issued_at: "2026-06-01T00:00:00Z".into(),
                expiration_time: "2026-07-01T00:00:00Z".into(),
                ..Default::default()
            })
            .unwrap(),
        );

        let sub_h = pay_kit::mpp::format_www_authenticate(&sub_challenge).unwrap();
        let auth_h = pay_kit::mpp::format_www_authenticate(&auth_challenge).unwrap();

        let hit = pick_authenticate_challenge(vec![sub_h.clone(), auth_h.clone()])
            .expect("authenticate present");
        assert_eq!(hit.intent.as_str(), "authenticate");

        // Header order shouldn't matter.
        let hit = pick_authenticate_challenge(vec![auth_h, sub_h]).expect("present");
        assert_eq!(hit.intent.as_str(), "authenticate");
    }

    #[test]
    fn pick_authenticate_challenge_returns_none_when_only_subscription_present() {
        let sub_challenge = pay_kit::mpp::PaymentChallenge::with_challenge_binding_secret(
            "test-secret",
            "test-realm",
            "solana",
            "subscription",
            pay_kit::mpp::Base64UrlJson::from_typed(&pay_kit::mpp::SubscriptionRequest {
                amount: "1".into(),
                currency: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
                period_unit: pay_kit::mpp::SubscriptionPeriodUnit::Day,
                period_count: "30".into(),
                recipient: "6ayEJCQB7gwzwdbWLi65DR9RTRTrS3QunK6j9h2WjQjW".into(),
                ..Default::default()
            })
            .unwrap(),
        );
        let h = pay_kit::mpp::format_www_authenticate(&sub_challenge).unwrap();
        assert!(pick_authenticate_challenge(vec![h]).is_none());
    }

    #[test]
    fn pick_authenticate_challenge_skips_malformed_entries() {
        let auth_challenge = pay_kit::mpp::PaymentChallenge::with_challenge_binding_secret(
            "test-secret",
            "test-realm",
            "solana",
            "authenticate",
            pay_kit::mpp::Base64UrlJson::from_typed(&pay_kit::mpp::AuthenticateRequest {
                domain: "api.example.com".into(),
                uri: "https://api.example.com/v1".into(),
                version: pay_kit::mpp::SIWMPP_VERSION.into(),
                nonce: "abc".into(),
                issued_at: "2026-06-01T00:00:00Z".into(),
                expiration_time: "2026-07-01T00:00:00Z".into(),
                ..Default::default()
            })
            .unwrap(),
        );
        let auth_h = pay_kit::mpp::format_www_authenticate(&auth_challenge).unwrap();

        let hit =
            pick_authenticate_challenge(vec!["not a www-authenticate header".to_string(), auth_h])
                .expect("authenticate present");
        assert_eq!(hit.intent.as_str(), "authenticate");
    }
}
