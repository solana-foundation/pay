//! Server-side bridge to the MPP `authenticate` intent.
//!
//! Builds a [`pay_kit::mpp::AuthenticateServer`] from pay's endpoint
//! config so the middleware can emit / verify SIWS-style identity
//! tokens that gate subscription endpoints between billing periods
//! without re-prompting the wallet.
//!
//! Composes with [`super::subscription`]: every subscription-gated
//! endpoint also accepts authenticate-intent credentials on
//! subsequent requests within the period.

use pay_kit::mpp::server::{AuthenticateConfig, AuthenticateServer};

use crate::{Error, Result};
use pay_types::metering::{SubscriptionEndpoint, SubscriptionPeriodUnit as TypesPeriodUnit};

use super::subscription::OperatorDefaults;

/// Build an [`AuthenticateServer`] for a subscription-gated endpoint.
///
/// Returns `Ok(None)` when the endpoint hasn't been published yet
/// (`plan_id` missing) — the middleware should still emit a
/// subscription challenge in that case, just not an authenticate
/// challenge.
///
/// `domain` is the HTTP host the server gates (typically the request
/// `Host` header). `uri` is the canonical origin URL.
pub fn build_handler(
    spec: &SubscriptionEndpoint,
    defaults: OperatorDefaults<'_>,
    domain: &str,
    uri: &str,
) -> Result<Option<AuthenticateServer>> {
    let Some(plan_id) = spec.plan_id.clone() else {
        return Ok(None);
    };
    let Some(plan_created_at) = spec.plan_created_at else {
        return Ok(None);
    };
    let (period_unit, period_count) = spec.parse_period().map_err(Error::Config)?;
    let period_hours = period_hours_from_spec(period_unit, period_count)?;

    // `challenge_binding_secret` and `realm` are required on the SDK side, no
    // SDK-internal env-var fallbacks. The pay server middleware
    // resolves both from `operator.{challenge_binding_secret,realm}`
    // in the server YAML (falling back to the subdomain for realm).
    let challenge_binding_secret = defaults
        .challenge_binding_secret
        .map(str::to_string)
        .ok_or_else(|| {
            Error::Config(
                "authenticate handler requires a challenge-binding secret — set \
             `operator.challenge_binding_secret` in the server YAML"
                    .into(),
            )
        })?;
    let realm = defaults
        .realm
        .map(str::to_string)
        .ok_or_else(|| Error::Config("authenticate handler requires a realm".into()))?;

    let config = AuthenticateConfig {
        domain: domain.to_string(),
        uri: uri.to_string(),
        plan_id,
        plan_created_at,
        period_hours,
        subscription_expires: spec.expires_at.clone(),
        network: defaults.network.to_string(),
        program_id: None,
        challenge_binding_secret,
        realm,
        statement: Some("Signed message to use active subscription.".to_string()),
        store: None,
    };

    AuthenticateServer::new(config)
        .map(Some)
        .map_err(|e| Error::Mpp(format!("Failed to initialise AuthenticateServer: {e}")))
}

/// Outcome of [`verify_live_subscriber`].
#[derive(Debug)]
pub enum LegacyAuthentication {
    /// The credential proves a wallet holding a live, paid subscription.
    Live(solana_pubkey::Pubkey),
    /// The credential is not a valid identity proof for this endpoint.
    InvalidCredential,
    /// The wallet holds no live subscription on-chain.
    NotSubscribed { message: String, retryable: bool },
}

/// Verify a legacy `authenticate` credential and require the wallet it proves
/// to hold a live, paid subscription to the endpoint's plan on-chain. The
/// identity proof alone never authorizes gated work.
pub async fn verify_live_subscriber(
    spec: &SubscriptionEndpoint,
    defaults: OperatorDefaults<'_>,
    domain: &str,
    uri: &str,
    description: Option<&str>,
    credential: &pay_kit::mpp::PaymentCredential,
) -> Result<LegacyAuthentication> {
    let Some(identity) = build_handler(spec, defaults.clone(), domain, uri)? else {
        return Ok(LegacyAuthentication::InvalidCredential);
    };
    let Ok(subscriber) = identity.verify(credential) else {
        return Ok(LegacyAuthentication::InvalidCredential);
    };
    let server = super::subscription::build_handler(spec, defaults, description)?;
    let (amount_base_units, _decimals, _mint) = super::subscription::resolve_amount(spec)?;
    Ok(
        match server
            .verify_active_subscription(&subscriber, &amount_base_units)
            .await
        {
            Ok(_) => LegacyAuthentication::Live(subscriber),
            Err(error) => LegacyAuthentication::NotSubscribed {
                message: error.message,
                retryable: error.retryable,
            },
        },
    )
}

/// Map pay's typed period unit + count to the SDK's `period_hours`.
fn period_hours_from_spec(unit: TypesPeriodUnit, count: u32) -> Result<u64> {
    if count == 0 {
        return Err(Error::Config(
            "subscription period count must be > 0".into(),
        ));
    }
    let hours = match unit {
        TypesPeriodUnit::Day => (count as u64).saturating_mul(24),
        TypesPeriodUnit::Week => (count as u64).saturating_mul(168),
    };
    if hours == 0 || hours > 8760 {
        return Err(Error::Config(format!(
            "subscription period_hours {hours} out of [1, 8760]"
        )));
    }
    Ok(hours)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_spec(plan_id: Option<&str>) -> SubscriptionEndpoint {
        SubscriptionEndpoint {
            plan_id: plan_id.map(str::to_string),
            plan_created_at: Some(1_780_000_000),
            plan_id_numeric: None,
            plan_bump: None,
            currency: "USDC".into(),
            price_usd: Some(9.99),
            amount_base_units: None,
            period: "30d".into(),
            recipient: None,
            puller: None,
            expires_at: None,
            free_trial_days: None,
        }
    }

    fn defaults<'a>() -> OperatorDefaults<'a> {
        OperatorDefaults {
            puller: "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
            recipient: "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
            network: "mainnet",
            rpc_url: "https://api.mainnet-beta.solana.com",
            challenge_binding_secret: Some("test-secret-key-do-not-use-32b-pad"),
            realm: Some("api.example.com"),
            fee_payer: false,
            fee_payer_signer: None,
            store: Some(std::sync::Arc::new(pay_kit::mpp::store::MemoryStore::new())),
        }
    }

    #[test]
    fn build_handler_returns_none_when_plan_id_missing() {
        let server = build_handler(
            &make_spec(None),
            defaults(),
            "api.example.com",
            "https://api.example.com/",
        )
        .expect("ok");
        assert!(server.is_none());
    }

    #[test]
    fn build_handler_returns_server_when_spec_complete() {
        let spec = make_spec(Some("Amp9FrnEX17tVeZ7QnHX1Hh4TynhH4sXLRSde797vdKR"));
        let server = build_handler(
            &spec,
            defaults(),
            "api.example.com",
            "https://api.example.com/",
        )
        .expect("ok");
        assert!(server.is_some());
    }

    async fn signed_credential(
        server: &AuthenticateServer,
        plan_id: &str,
    ) -> pay_kit::mpp::PaymentCredential {
        use pay_kit::mpp::program::subscriptions::{default_program_id, find_subscription_pda};
        use pay_kit::mpp::solana_keychain::SolanaSigner;
        let key = ed25519_dalek::SigningKey::from_bytes(&[11; 32]);
        let mut keypair = [0u8; 64];
        keypair[..32].copy_from_slice(&key.to_bytes());
        keypair[32..].copy_from_slice(key.verifying_key().as_bytes());
        let signer =
            pay_kit::mpp::solana_keychain::memory::MemorySigner::from_bytes(&keypair).unwrap();
        let plan = plan_id.parse().unwrap();
        let (pda, _) = find_subscription_pda(&plan, &signer.pubkey(), &default_program_id());
        let header = pay_kit::mpp::client::build_authenticate_credential_header(
            &signer,
            &server.challenge().unwrap(),
            &pda.to_string(),
        )
        .await
        .unwrap();
        pay_kit::mpp::parse_authorization(&header).unwrap()
    }

    fn rfc3339_from_now(seconds: i64) -> String {
        (chrono::Utc::now() + chrono::Duration::seconds(seconds))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    #[tokio::test]
    async fn legacy_tokens_stop_verifying_at_the_endpoint_expiry() {
        let plan_id = "Amp9FrnEX17tVeZ7QnHX1Hh4TynhH4sXLRSde797vdKR";
        let mut spec = make_spec(Some(plan_id));
        let server = |spec: &SubscriptionEndpoint| {
            build_handler(
                spec,
                defaults(),
                "api.example.com",
                "https://api.example.com/",
            )
            .unwrap()
            .unwrap()
        };
        // Issued before the cutoff, so it carries the plan-period expiration.
        let credential = signed_credential(&server(&spec), plan_id).await;
        server(&spec).verify(&credential).unwrap();

        // A cutoff strictly before the credential's period end, however
        // close the wall clock is to it.
        let period_secs = 30 * 86_400;
        let plan_created_at = spec.plan_created_at.unwrap();
        let now = chrono::Utc::now().timestamp();
        let period_end =
            plan_created_at + ((now - plan_created_at) / period_secs + 1) * period_secs;
        spec.expires_at = Some(rfc3339_from_now(((period_end - now) / 2).clamp(1, 3600)));
        server(&spec).verify(&credential).unwrap_err();
        spec.expires_at = Some(rfc3339_from_now(-1));
        server(&spec).verify(&credential).unwrap_err();
    }

    #[tokio::test]
    async fn tokens_issued_under_an_endpoint_expiry_verify_until_it() {
        let plan_id = "Amp9FrnEX17tVeZ7QnHX1Hh4TynhH4sXLRSde797vdKR";
        let mut spec = make_spec(Some(plan_id));
        spec.expires_at = Some(rfc3339_from_now(3600));
        let server = build_handler(
            &spec,
            defaults(),
            "api.example.com",
            "https://api.example.com/",
        )
        .unwrap()
        .unwrap();
        let credential = signed_credential(&server, plan_id).await;
        server.verify(&credential).unwrap();
    }

    const PLAN_ID: &str = "Amp9FrnEX17tVeZ7QnHX1Hh4TynhH4sXLRSde797vdKR";
    const AMOUNT: u64 = 9_990_000;
    const INIT_ID: i64 = 7;

    fn test_subscriber() -> solana_pubkey::Pubkey {
        use pay_kit::mpp::solana_keychain::SolanaSigner;
        let key = ed25519_dalek::SigningKey::from_bytes(&[11; 32]);
        let mut keypair = [0u8; 64];
        keypair[..32].copy_from_slice(&key.to_bytes());
        keypair[32..].copy_from_slice(key.verifying_key().as_bytes());
        pay_kit::mpp::solana_keychain::memory::MemorySigner::from_bytes(&keypair)
            .unwrap()
            .pubkey()
    }

    /// On-chain `SubscriptionDelegation` for the test subscriber.
    fn delegation_bytes(amount_pulled_in_period: u64) -> Vec<u8> {
        let plan: solana_pubkey::Pubkey = PLAN_ID.parse().unwrap();
        let mut data = vec![4, 1, 255];
        data.extend_from_slice(test_subscriber().as_ref());
        data.extend_from_slice(plan.as_ref());
        data.extend_from_slice(solana_pubkey::Pubkey::new_unique().as_ref());
        data.extend_from_slice(&INIT_ID.to_le_bytes());
        data.extend_from_slice(&AMOUNT.to_le_bytes());
        data.extend_from_slice(&720u64.to_le_bytes());
        data.extend_from_slice(&1_700_000_000i64.to_le_bytes());
        data.extend_from_slice(&amount_pulled_in_period.to_le_bytes());
        data.extend_from_slice(&(chrono::Utc::now().timestamp() - 60).to_le_bytes());
        data.extend_from_slice(&0i64.to_le_bytes());
        data
    }

    fn authority_bytes() -> Vec<u8> {
        let mut data = vec![0u8; 106];
        data[98..].copy_from_slice(&INIT_ID.to_le_bytes());
        data
    }

    /// A JSON-RPC node serving `accounts`, owned by the subscriptions program.
    async fn account_rpc(accounts: Vec<(solana_pubkey::Pubkey, Vec<u8>)>) -> String {
        use axum::{Json, Router, routing::post};
        use base64::Engine as _;
        let accounts: std::collections::HashMap<String, Vec<u8>> = accounts
            .into_iter()
            .map(|(address, data)| (address.to_string(), data))
            .collect();
        let owner = pay_kit::mpp::program::subscriptions::default_program_id().to_string();
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<serde_json::Value>| {
                let value = accounts
                    .get(request["params"][0].as_str().unwrap_or_default())
                    .map(|data| {
                        serde_json::json!({
                            "data": [base64::engine::general_purpose::STANDARD.encode(data), "base64"],
                            "executable": false, "lamports": 1, "owner": owner.clone(),
                            "rentEpoch": 0, "space": data.len()
                        })
                    });
                async move {
                    assert_eq!(request["method"], "getAccountInfo");
                    Json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {"context": {"slot": 1}, "value": value},
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    async fn legacy_outcome(
        accounts: Vec<(solana_pubkey::Pubkey, Vec<u8>)>,
    ) -> LegacyAuthentication {
        let rpc_url = account_rpc(accounts).await;
        let spec = make_spec(Some(PLAN_ID));
        let defaults = OperatorDefaults {
            rpc_url: &rpc_url,
            ..defaults()
        };
        let identity = build_handler(
            &spec,
            defaults.clone(),
            "api.example.com",
            "https://api.example.com/",
        )
        .unwrap()
        .unwrap();
        let credential = signed_credential(&identity, PLAN_ID).await;
        verify_live_subscriber(
            &spec,
            defaults,
            "api.example.com",
            "https://api.example.com/",
            None,
            &credential,
        )
        .await
        .unwrap()
    }

    fn subscription_accounts(
        amount_pulled_in_period: u64,
    ) -> Vec<(solana_pubkey::Pubkey, Vec<u8>)> {
        use pay_kit::mpp::program::subscriptions::{
            default_program_id, find_subscription_authority_pda, find_subscription_pda,
        };
        let program = default_program_id();
        let subscriber = test_subscriber();
        let mint = pay_types::stablecoin_mints::USDC_MAINNET.parse().unwrap();
        vec![
            (
                find_subscription_pda(&PLAN_ID.parse().unwrap(), &subscriber, &program).0,
                delegation_bytes(amount_pulled_in_period),
            ),
            (
                find_subscription_authority_pda(&subscriber, &mint, &program).0,
                authority_bytes(),
            ),
        ]
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn legacy_identity_without_a_subscription_is_not_authorized() {
        let outcome = legacy_outcome(Vec::new()).await;
        assert!(
            matches!(&outcome, LegacyAuthentication::NotSubscribed { message, .. } if message.contains("not found"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn legacy_identity_with_an_unpaid_period_is_not_authorized() {
        let outcome = legacy_outcome(subscription_accounts(0)).await;
        assert!(matches!(
            outcome,
            LegacyAuthentication::NotSubscribed { .. }
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn legacy_identity_with_a_live_subscription_is_authorized() {
        let outcome = legacy_outcome(subscription_accounts(AMOUNT)).await;
        assert!(
            matches!(outcome, LegacyAuthentication::Live(subscriber) if subscriber == test_subscriber())
        );
    }

    #[tokio::test]
    async fn tokens_do_not_verify_at_another_endpoint() {
        let spec = make_spec(Some(PLAN_ID));
        let server = |uri: &str| {
            build_handler(&spec, defaults(), "api.example.com", uri)
                .unwrap()
                .unwrap()
        };
        let feed = server("https://api.example.com/v1/feed");
        let credential = signed_credential(&feed, PLAN_ID).await;
        feed.verify(&credential).unwrap();
        assert!(
            server("https://api.example.com/v1/admin")
                .verify(&credential)
                .is_err()
        );
        assert!(
            build_handler(
                &spec,
                defaults(),
                "other.example.com",
                "https://api.example.com/v1/feed"
            )
            .unwrap()
            .unwrap()
            .verify(&credential)
            .is_err()
        );
    }

    #[test]
    fn malformed_endpoint_expiry_is_a_config_error() {
        let mut spec = make_spec(Some("Amp9FrnEX17tVeZ7QnHX1Hh4TynhH4sXLRSde797vdKR"));
        spec.expires_at = Some("next tuesday".into());
        assert!(
            build_handler(
                &spec,
                defaults(),
                "api.example.com",
                "https://api.example.com/"
            )
            .is_err()
        );
    }

    #[test]
    fn period_hours_rejects_zero_count() {
        assert!(period_hours_from_spec(TypesPeriodUnit::Day, 0).is_err());
    }

    #[test]
    fn period_hours_rejects_out_of_range() {
        assert!(period_hours_from_spec(TypesPeriodUnit::Day, 366).is_err());
    }

    #[test]
    fn period_hours_maps_day_and_week_units() {
        assert_eq!(
            period_hours_from_spec(TypesPeriodUnit::Day, 30).unwrap(),
            720
        );
        assert_eq!(
            period_hours_from_spec(TypesPeriodUnit::Week, 4).unwrap(),
            672
        );
    }
}
