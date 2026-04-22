use serde::{Deserialize, Serialize};

pub mod metering;
pub mod registry;
pub mod splits;

/// Represents an HTTP 402 payment challenge returned by a server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentChallenge {
    /// The URL that requires payment.
    pub resource_url: String,
    /// The payment endpoint to submit payment to.
    pub payment_url: String,
    /// Amount required in the smallest unit (e.g., satoshis, lamports).
    pub amount: u64,
    /// Currency or token identifier (e.g., "USD", "SOL", "BTC").
    pub currency: String,
    /// Human-readable description of what is being purchased.
    #[serde(default)]
    pub description: Option<String>,
}

/// The result of a successful payment, containing a receipt token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentReceipt {
    /// Opaque token proving payment was made.
    pub token: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payment_challenge_serde_roundtrip() {
        let challenge = PaymentChallenge {
            resource_url: "https://api.example.com/data".to_string(),
            payment_url: "https://pay.example.com".to_string(),
            amount: 1000,
            currency: "USDC".to_string(),
            description: Some("API access".to_string()),
        };
        let json = serde_json::to_string(&challenge).unwrap();
        let back: PaymentChallenge = serde_json::from_str(&json).unwrap();
        assert_eq!(back.resource_url, challenge.resource_url);
        assert_eq!(back.payment_url, challenge.payment_url);
        assert_eq!(back.amount, challenge.amount);
        assert_eq!(back.currency, challenge.currency);
        assert_eq!(back.description, challenge.description);
    }

    #[test]
    fn payment_challenge_without_description() {
        let json = r#"{"resource_url":"https://a.com","payment_url":"https://b.com","amount":500,"currency":"SOL"}"#;
        let challenge: PaymentChallenge = serde_json::from_str(json).unwrap();
        assert_eq!(challenge.amount, 500);
        assert!(challenge.description.is_none());
    }

    #[test]
    fn payment_receipt_serde_roundtrip() {
        let receipt = PaymentReceipt {
            token: "receipt_token_123".to_string(),
        };
        let json = serde_json::to_string(&receipt).unwrap();
        let back: PaymentReceipt = serde_json::from_str(&json).unwrap();
        assert_eq!(back.token, receipt.token);
    }
}
