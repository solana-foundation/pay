use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tenant {
    pub payer: String,
    pub key: String,
    pub channel_id: String,
}

fn default_driver() -> String {
    "privy".into()
}
fn default_chain() -> String {
    "solana".into()
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct CreateWalletRequest {
    #[serde(default = "default_driver")]
    pub driver: String,
    /// Stable payer-local name. Reusing it returns the same wallet.
    pub name: String,
    #[serde(default = "default_chain")]
    pub chain: String,
    /// Human-readable role such as `tax`, `profit`, or `infrastructure`.
    pub purpose: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct WalletRequest {
    #[serde(default = "default_driver")]
    pub driver: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Wallet {
    pub driver: String,
    pub id: String,
    pub name: String,
    pub chain: String,
    pub address: String,
    pub purpose: Option<String>,
    /// Wallets can receive funds after the provisioning channel closes and
    /// are therefore retained until an explicit future recovery flow exists.
    pub lifecycle: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct DriverCapabilities {
    pub driver: String,
    pub display_name: String,
    pub chains: Vec<String>,
    pub operations: Vec<String>,
    pub custody: String,
}
