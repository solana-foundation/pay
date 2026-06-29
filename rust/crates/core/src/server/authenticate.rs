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

    let _ = spec; // `spec` is only consulted via `parse_period` above; reserved for richer statement composition.
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
