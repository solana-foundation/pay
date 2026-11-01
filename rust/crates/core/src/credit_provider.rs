//! Merchant-wide credit-provider policy, independent of payment intent.

use crate::{Error, Result};
use pay_types::metering::{CreditProvider, OperatorConfig};

/// Resolve named providers through the SDK before advertising payment options.
pub fn validate_operator(config: &OperatorConfig) -> Result<()> {
    let mut providers = Vec::with_capacity(config.credit_providers.len());
    for provider in &config.credit_providers {
        match provider {
            CreditProvider::Coinflow => providers.push(pay_kit::CreditProvider::Coinflow),
            // Fails closed in Pay itself, independent of the pinned pay-kit
            // revision: no reviewed deployment exists and pay-kit does not yet
            // emit `*WithHooks` channel instructions. Never fall back to hook JSON.
            CreditProvider::Musebook => {
                return Err(Error::Config(
                    "credit provider musebook is unavailable: no reviewed issuer integration is registered"
                        .to_string(),
                ));
            }
        }
    }
    pay_kit::validate_credit_providers(&providers).map_err(|error| Error::Config(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_selection_is_beside_currencies_not_session_settings() {
        let config: OperatorConfig = serde_json::from_value(serde_json::json!({
            "currencies": {"usd": ["USDC", "USDT"]},
            "credit_providers": ["coinflow"]
        }))
        .unwrap();
        assert_eq!(config.credit_providers, [CreditProvider::Coinflow]);
        // A provider name is not evidence that its adapter has been reviewed.
        assert!(validate_operator(&config).is_err());
    }

    #[test]
    fn existing_configs_default_to_no_credit_providers() {
        let config: OperatorConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(config.credit_providers.is_empty());
        assert!(validate_operator(&config).is_ok());
        assert!(
            serde_json::to_value(config)
                .unwrap()
                .get("credit_providers")
                .is_none()
        );
    }

    #[test]
    fn musebook_is_named_selection_and_fails_closed() {
        let config: OperatorConfig = serde_json::from_value(serde_json::json!({
            "currencies": {"usd": ["USDC"]},
            "credit_providers": ["musebook"]
        }))
        .unwrap();
        assert_eq!(config.credit_providers, [CreditProvider::Musebook]);
        let error = validate_operator(&config).unwrap_err().to_string();
        assert!(error.contains("musebook is unavailable"), "{error}");
        let mixed: OperatorConfig = serde_json::from_value(serde_json::json!({
            "credit_providers": ["coinflow", "musebook"]
        }))
        .unwrap();
        assert!(validate_operator(&mixed).is_err());
    }

    #[test]
    fn provider_selection_never_accepts_hook_json() {
        for value in [
            serde_json::json!({"credit_providers": [{"musebook": {"program": "x"}}]}),
            serde_json::json!({"credit_providers": [{"hooks": []}]}),
            serde_json::json!({"credit_providers": ["Musebook"]}),
            serde_json::json!({"credit_providers": ["musebook.trade"]}),
        ] {
            assert!(serde_json::from_value::<OperatorConfig>(value).is_err());
        }
    }

    #[test]
    fn unknown_provider_names_are_rejected() {
        assert!(
            serde_json::from_value::<OperatorConfig>(serde_json::json!({
                "credit_providers": ["unreviewed"]
            }))
            .is_err()
        );
    }
}
