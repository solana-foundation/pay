use serde::{Deserialize, Serialize};

pub mod metering;

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
