//! MCP payment permissions backed by pay-kit's reusable permission model.

use std::path::Path;

use pay_kit::client::{
    AssetPermission, ClientPermissions, OriginPermissionOverride, PaymentCandidate, SolanaNetwork,
    UsdAmount,
};
use serde::Deserialize;

use pay_core::client::{mpp, x402};

/// YAML/CLI representation of MCP payment permissions.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionConfig {
    /// Exact HTTP(S) origins that may request payment. Omit to allow any origin.
    #[serde(default)]
    pub origins: Option<Vec<String>>,
    /// Solana clusters that may receive payments. Omit for mainnet only.
    #[serde(default)]
    pub networks: Option<Vec<String>>,
    /// Global stablecoin cap, such as `"$1.00"`.
    #[serde(default)]
    pub max_payment: Option<String>,
    /// Remove the global stablecoin cap.
    #[serde(default)]
    pub unlimited_amount: bool,
    /// Permit mints other than pay-kit's known stablecoins.
    #[serde(default)]
    pub allow_any_asset: bool,
    /// Explicitly permitted assets and optional atomic caps.
    #[serde(default)]
    pub assets: Vec<AssetConfig>,
    /// Per-origin cap overrides. These never add an origin to `origins`.
    #[serde(default)]
    pub origin_overrides: Vec<OriginOverrideConfig>,
}

/// One explicitly permitted SPL asset.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetConfig {
    pub network: String,
    pub asset: String,
    #[serde(default)]
    pub max_payment_atomic: Option<u64>,
}

/// Overrides for one exact origin.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginOverrideConfig {
    pub origin: String,
    #[serde(default)]
    pub max_payment: Option<String>,
    #[serde(default)]
    pub unlimited_amount: bool,
    #[serde(default)]
    pub asset_caps: Vec<AssetConfig>,
}

impl PermissionConfig {
    /// Load a strict YAML permission file.
    pub fn from_yaml_file(path: &Path) -> Result<Self, String> {
        let source = std::fs::read_to_string(path).map_err(|error| {
            format!("Could not read MCP permissions {}: {error}", path.display())
        })?;
        serde_yml::from_str(&source)
            .map_err(|error| format!("Invalid MCP permissions {}: {error}", path.display()))
    }

    /// Validate and compile the configuration with pay-kit's builder.
    pub fn build(&self) -> Result<ClientPermissions, String> {
        if self.max_payment.is_some() && self.unlimited_amount {
            return Err("MCP permissions cannot set both max_payment and unlimited_amount".into());
        }

        let mut builder = ClientPermissions::builder();
        if let Some(origins) = &self.origins {
            if origins.is_empty() {
                return Err("MCP permissions origins must not be empty".into());
            }
            for origin in origins {
                builder = builder.allow_origin(origin).map_err(|e| e.to_string())?;
            }
        }
        if let Some(networks) = &self.networks {
            let (first, rest) = networks
                .split_first()
                .ok_or_else(|| "MCP permissions networks must not be empty".to_string())?;
            builder = builder.only_network(parse_network(first)?);
            for network in rest {
                builder = builder.allow_network(parse_network(network)?);
            }
        }
        if let Some(cap) = &self.max_payment {
            builder = builder.max_amount_per_payment(parse_usd(cap)?);
        } else if self.unlimited_amount {
            builder = builder.without_amount_cap();
        }
        if self.allow_any_asset {
            builder = builder.allow_any_asset();
        }
        for asset in &self.assets {
            builder = builder.allow_asset(asset.permission()?);
        }
        for entry in &self.origin_overrides {
            if entry.max_payment.is_some() && entry.unlimited_amount {
                return Err(format!(
                    "origin override {} cannot set both max_payment and unlimited_amount",
                    entry.origin
                ));
            }
            let mut override_builder = OriginPermissionOverride::builder(&entry.origin);
            if let Some(cap) = &entry.max_payment {
                override_builder = override_builder.max_amount_per_payment(parse_usd(cap)?);
            } else if entry.unlimited_amount {
                override_builder = override_builder.without_amount_cap();
            }
            for asset in &entry.asset_caps {
                let cap = asset.max_payment_atomic.ok_or_else(|| {
                    format!(
                        "origin override asset {} requires max_payment_atomic",
                        asset.asset
                    )
                })?;
                override_builder = override_builder
                    .asset_cap(parse_network(&asset.network)?, &asset.asset, cap)
                    .map_err(|e| e.to_string())?;
            }
            builder = builder.override_origin(override_builder.build().map_err(|e| e.to_string())?);
        }
        builder.build().map_err(|e| e.to_string())
    }
}

impl AssetConfig {
    fn permission(&self) -> Result<AssetPermission, String> {
        let network = parse_network(&self.network)?;
        match self.max_payment_atomic {
            Some(cap) => AssetPermission::with_cap(network, &self.asset, cap),
            None => AssetPermission::new(network, &self.asset),
        }
        .map_err(|e| e.to_string())
    }
}

fn parse_usd(value: &str) -> Result<UsdAmount, String> {
    value.parse::<UsdAmount>().map_err(|e| e.to_string())
}

fn parse_network(value: &str) -> Result<SolanaNetwork, String> {
    match value {
        "mainnet" | "mainnet-beta" | "solana:mainnet" => Ok(SolanaNetwork::Mainnet),
        "devnet" | "solana:devnet" => Ok(SolanaNetwork::Devnet),
        "localnet" | "localhost" => Ok(SolanaNetwork::Localnet),
        value => pay_kit::x402::exact::cluster_for_caip2_network(value)
            .and_then(|cluster| cluster.parse().ok())
            .ok_or_else(|| format!("invalid Solana network `{value}`")),
    }
}

/// Compiled permission checks attached to a local MCP call.
#[derive(Debug, Clone)]
pub struct McpPermissions(ClientPermissions);

impl McpPermissions {
    pub fn from_config(config: &PermissionConfig) -> Result<Self, String> {
        config.build().map(Self)
    }

    fn authorize(
        &self,
        resource_url: &str,
        network: &str,
        asset: &str,
        amount: u64,
    ) -> Result<(), pay_core::Error> {
        let network = parse_network(network).map_err(permission_error)?;
        let mint =
            pay_kit::mpp::protocol::solana::resolve_stablecoin_mint(asset, Some(network.as_str()))
                .unwrap_or(asset);
        self.0
            .authorize(&PaymentCandidate::new(resource_url, network, mint, amount))
            .map(|_| ())
            .map_err(|error| permission_error(error.message))
    }

    pub fn authorize_mpp(
        &self,
        challenge: &mpp::Challenge,
        resource_url: &str,
    ) -> Result<(), pay_core::Error> {
        let request: pay_kit::mpp::ChargeRequest = challenge
            .request
            .decode()
            .map_err(|error| permission_error(format!("invalid MPP charge: {error}")))?;
        let details: pay_kit::mpp::protocol::solana::MethodDetails = request
            .method_details
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| permission_error(format!("invalid MPP method details: {error}")))?
            .unwrap_or_default();
        let network = details.network.as_deref().unwrap_or("mainnet");
        let amount = request
            .amount
            .parse()
            .map_err(|_| permission_error("MPP amount is not an unsigned integer"))?;
        self.authorize(resource_url, network, &request.currency, amount)
    }

    pub fn authorize_x402(
        &self,
        challenge: &x402::Challenge,
        resource_url: &str,
    ) -> Result<(), pay_core::Error> {
        let requirement = &challenge.requirements;
        let network = requirement
            .cluster
            .as_deref()
            .unwrap_or(&requirement.network);
        let amount = requirement
            .amount
            .parse()
            .map_err(|_| permission_error("x402 amount is not an unsigned integer"))?;
        self.authorize(resource_url, network, &requirement.currency, amount)
    }

    pub fn authorize_upto(
        &self,
        challenge: &x402::UptoChallenge,
        resource_url: &str,
    ) -> Result<(), pay_core::Error> {
        let requirement = &challenge.requirements;
        self.authorize(
            resource_url,
            &requirement.network,
            &requirement.asset,
            requirement
                .max_amount()
                .map_err(|error| permission_error(error.to_string()))?,
        )
    }

    pub fn authorize_batch(
        &self,
        challenge: &x402::BatchChallenge,
        resource_url: &str,
    ) -> Result<(), pay_core::Error> {
        let requirement = &challenge.requirements;
        self.authorize(
            resource_url,
            &requirement.network,
            &requirement.asset,
            requirement
                .amount()
                .map_err(|error| permission_error(error.to_string()))?,
        )
    }

    pub fn reject_unsupported(&self, kind: &str) -> Result<(), pay_core::Error> {
        Err(permission_error(format!(
            "{kind} is not supported when MCP payment permissions are enabled"
        )))
    }
}

fn permission_error(message: impl Into<String>) -> pay_core::Error {
    pay_core::Error::PaymentRejected(format!("MCP permission denied: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_builds_the_same_permission_model_as_cli_flags() {
        let config: PermissionConfig = serde_yml::from_str(
            r#"
origins:
  - https://api.example.com/path
networks: [mainnet]
max_payment: "$2.50"
assets:
  - network: mainnet
    asset: 11111111111111111111111111111111
    max_payment_atomic: 42
"#,
        )
        .unwrap();
        let permissions = McpPermissions::from_config(&config).unwrap();
        assert!(
            permissions
                .authorize(
                    "https://api.example.com/report",
                    "mainnet",
                    "USDC",
                    2_500_000
                )
                .is_ok()
        );
        assert!(
            permissions
                .authorize("https://other.example/report", "mainnet", "USDC", 1)
                .is_err()
        );
    }

    #[test]
    fn unknown_yaml_fields_are_rejected() {
        let error = serde_yml::from_str::<PermissionConfig>("maximum: 1").unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }
}
