//! MCP payment permissions backed by pay-kit's reusable permission model.

use std::path::Path;

use pay_kit::client::{
    AssetPermission, ClientPermissions, OriginPermissionOverride, PaymentCandidate,
    PaymentDestination, PaymentTerms, SolanaNetwork, UsdAmount,
};
use pay_kit::mpp::client::SessionPayout;
use serde::Deserialize;
use solana_pubkey::Pubkey;

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
    /// Wallets a payment may transfer to, as its primary recipient or any
    /// split. Omit to allow any recipient; approvals still name every one.
    #[serde(default)]
    pub recipients: Option<Vec<String>>,
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
        if let Some(recipients) = &self.recipients {
            if recipients.is_empty() {
                return Err("MCP permissions recipients must not be empty".into());
            }
            for recipient in recipients {
                let recipient = recipient
                    .parse::<Pubkey>()
                    .map_err(|_| format!("invalid MCP permission recipient `{recipient}`"))?;
                builder = builder.allow_recipient(recipient);
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
        value => match pay_kit::x402::exact::cluster_for_caip2_network(value) {
            Some("mainnet-beta") => Ok(SolanaNetwork::Mainnet),
            Some(cluster) => cluster
                .parse()
                .map_err(|_| format!("invalid Solana network `{value}`")),
            None => Err(format!("invalid Solana network `{value}`")),
        },
    }
}

/// Which wallets a hosted tenant's payments may reach, as the primary
/// recipient or any split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovedRecipients {
    /// The tenant approved no specific recipients: the server names who is
    /// paid. Every approval still lists each destination and its share.
    Any,
    /// Only these wallets may receive any leg of a payment.
    Only(Vec<Pubkey>),
}

/// Compiled permission checks attached to a local MCP call.
#[derive(Debug, Clone)]
pub struct McpPermissions(ClientPermissions);

impl McpPermissions {
    pub fn from_config(config: &PermissionConfig) -> Result<Self, String> {
        config.build().map(Self)
    }

    /// Permissions for hosted tenants: mainnet and known stablecoins only,
    /// from any origin, paying only `recipients`. Amounts are bounded by the
    /// tenant spending policy.
    pub fn hosted(recipients: &ApprovedRecipients) -> Self {
        let recipients = match recipients {
            ApprovedRecipients::Any => None,
            ApprovedRecipients::Only(wallets) => {
                Some(wallets.iter().map(Pubkey::to_string).collect())
            }
        };
        Self::from_config(&PermissionConfig {
            networks: Some(vec!["mainnet".to_string()]),
            recipients,
            unlimited_amount: true,
            ..Default::default()
        })
        .expect("hosted payment permissions are valid for any recipient list")
    }

    /// Authorize a payment to one recipient for its whole amount.
    fn authorize(
        &self,
        resource_url: &str,
        network: &str,
        asset: &str,
        amount: u64,
        recipient: &str,
    ) -> Result<(), pay_core::Error> {
        let recipient = parse_recipient(recipient)?;
        self.authorize_destinations(
            resource_url,
            network,
            asset,
            amount,
            &[PaymentDestination::new(recipient, amount)],
        )
    }

    /// Authorize a payment split across `destinations`, which must add up
    /// to `amount`.
    fn authorize_destinations(
        &self,
        resource_url: &str,
        network: &str,
        asset: &str,
        amount: u64,
        destinations: &[PaymentDestination],
    ) -> Result<(), pay_core::Error> {
        let network = parse_network(network).map_err(permission_error)?;
        let mint =
            pay_kit::mpp::protocol::solana::resolve_stablecoin_mint(asset, Some(network.as_str()))
                .unwrap_or(asset);
        self.authorize_candidate(&PaymentCandidate::new(
            resource_url,
            network,
            mint,
            amount,
            destinations,
        ))
    }

    fn authorize_candidate(&self, candidate: &PaymentCandidate<'_>) -> Result<(), pay_core::Error> {
        self.0
            .authorize(candidate)
            .map(|_| ())
            .map_err(|error| permission_error(error.message))
    }

    /// Authorize an MPP charge on the terms the builder pays: the primary
    /// recipient and every split leg, each checked against the recipient
    /// allowlist.
    pub fn authorize_mpp(
        &self,
        challenge: &mpp::Challenge,
        resource_url: &str,
    ) -> Result<(), pay_core::Error> {
        let terms = PaymentTerms::from_mpp_charge(challenge, resource_url)
            .map_err(|error| permission_error(error.message))?
            .ok_or_else(|| permission_error("MPP challenge is not a Solana stablecoin charge"))?;
        self.authorize_candidate(&terms.candidate())
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
        self.authorize(
            resource_url,
            network,
            &requirement.currency,
            amount,
            &requirement.recipient,
        )
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
            &requirement.pay_to,
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
            &requirement.pay_to,
        )
    }

    /// Authorize the deposit an operator-signed session open would lock and
    /// the payout it commits it to: the channel payee and every distribution
    /// split, each checked against the recipient allowlist. Returns that
    /// payout, which the opener must be held to.
    pub fn authorize_session(
        &self,
        challenge: &mpp::Challenge,
        resource_url: &str,
    ) -> Result<SessionPayout, pay_core::Error> {
        let request: pay_kit::mpp::SessionRequest = challenge
            .request
            .decode()
            .map_err(|error| permission_error(format!("invalid MPP session: {error}")))?;
        let deposit = pay_core::session::operator_session_deposit(&request)
            .map_err(|error| permission_error(error.to_string()))?;
        let payout = SessionPayout::from_request(&request)
            .map_err(|error| permission_error(format!("invalid MPP session payout: {error}")))?;
        self.authorize_destinations(
            resource_url,
            &request.method_details.network,
            &request.currency,
            deposit,
            &session_destinations(&payout, deposit)?,
        )?;
        Ok(payout)
    }

    /// Authorize the escrow a batch `open` or `top_up` would deposit.
    pub fn authorize_batch_escrow(
        &self,
        challenge: &x402::BatchChallenge,
        resource_url: &str,
        escrow: u64,
    ) -> Result<(), pay_core::Error> {
        let requirement = &challenge.requirements;
        self.authorize(
            resource_url,
            &requirement.network,
            &requirement.asset,
            escrow,
            &requirement.pay_to,
        )
    }

    pub fn reject_unsupported(&self, kind: &str) -> Result<(), pay_core::Error> {
        Err(permission_error(format!(
            "{kind} is not supported when MCP payment permissions are enabled"
        )))
    }
}

/// Split a session deposit the way the channel would distribute it: each
/// split its basis-point share, the payee the remainder.
fn session_destinations(
    payout: &SessionPayout,
    deposit: u64,
) -> Result<Vec<PaymentDestination>, pay_core::Error> {
    let shares: u32 = payout.splits.iter().map(|split| u32::from(split.bps)).sum();
    if shares > 10_000 {
        return Err(permission_error("MPP session splits exceed the deposit"));
    }
    let mut destinations = vec![PaymentDestination::new(payout.payee, 0)];
    let mut shared = 0u64;
    for split in &payout.splits {
        let share = u64::try_from(u128::from(deposit) * u128::from(split.bps) / 10_000)
            .map_err(|_| permission_error("MPP session split overflows"))?;
        shared = shared
            .checked_add(share)
            .filter(|shared| *shared <= deposit)
            .ok_or_else(|| permission_error("MPP session splits exceed the deposit"))?;
        destinations.push(PaymentDestination::new(split.recipient, share));
    }
    destinations[0].amount = deposit - shared;
    Ok(destinations)
}

fn parse_recipient(recipient: &str) -> Result<Pubkey, pay_core::Error> {
    recipient
        .parse()
        .map_err(|_| permission_error("payment recipient is not a valid Solana address"))
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
                    2_500_000,
                    PAYEE
                )
                .is_ok()
        );
        assert!(
            permissions
                .authorize("https://other.example/report", "mainnet", "USDC", 1, PAYEE)
                .is_err()
        );
    }

    #[test]
    fn unknown_yaml_fields_are_rejected() {
        let error = serde_yml::from_str::<PermissionConfig>("maximum: 1").unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    const PAYEE: &str = "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY";
    const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    #[test]
    fn hosted_permissions_allow_known_stablecoins_on_mainnet_only() {
        let hosted = McpPermissions::hosted(&ApprovedRecipients::Any);
        assert!(
            hosted
                .authorize(
                    "https://any.example/x",
                    "mainnet",
                    "USDC",
                    50_000_000,
                    PAYEE
                )
                .is_ok()
        );
        assert!(
            hosted
                .authorize("https://any.example/x", "mainnet", USDC, 1, PAYEE)
                .is_ok()
        );
        // Challenge-selected custom mints, native SOL, and other clusters
        // never reach the hosted signer.
        let custom = "So11111111111111111111111111111111111111112";
        assert!(
            hosted
                .authorize("https://any.example/x", "mainnet", custom, 1, PAYEE)
                .is_err()
        );
        assert!(
            hosted
                .authorize("https://any.example/x", "mainnet", "SOL", 1, PAYEE)
                .is_err()
        );
        assert!(
            hosted
                .authorize("https://any.example/x", "devnet", "USDC", 1, PAYEE)
                .is_err()
        );
        assert!(
            hosted
                .authorize("not a url", "mainnet", "USDC", 1, PAYEE)
                .is_err()
        );
    }

    const SPLIT: &str = "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin";

    /// A mainnet USDC charge paying `primary` and one split to `split`.
    fn split_charge(primary: &str, split: &str) -> mpp::Challenge {
        let request = serde_json::json!({
            "amount": "5000000",
            "currency": "USDC",
            "recipient": primary,
            "methodDetails": {
                "network": "mainnet",
                "decimals": 6,
                "tokenProgram": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                "splits": [{ "recipient": split, "amount": "4000000" }],
            },
        });
        pay_kit::mpp::PaymentChallenge::new(
            "id",
            "realm",
            "solana",
            "charge",
            pay_kit::mpp::Base64UrlJson::from_value(&request).unwrap(),
        )
    }

    fn only(recipients: &[&str]) -> McpPermissions {
        McpPermissions::hosted(&ApprovedRecipients::Only(
            recipients.iter().map(|r| r.parse().unwrap()).collect(),
        ))
    }

    #[test]
    fn hosted_charges_bind_the_primary_recipient_and_every_split() {
        let url = "https://merchant.example/paid";
        // A split to a wallet the tenant never approved is refused even
        // though the primary recipient and the total are in policy.
        let error = only(&[PAYEE])
            .authorize_mpp(&split_charge(PAYEE, SPLIT), url)
            .unwrap_err();
        assert!(error.to_string().contains(SPLIT), "{error}");
        // So is a primary recipient outside the list.
        assert!(
            only(&[SPLIT])
                .authorize_mpp(&split_charge(PAYEE, SPLIT), url)
                .is_err()
        );
        // Control: approving both legs admits the same charge.
        assert!(
            only(&[PAYEE, SPLIT])
                .authorize_mpp(&split_charge(PAYEE, SPLIT), url)
                .is_ok()
        );
        // A tenant without approved recipients pays whoever the server names.
        assert!(
            McpPermissions::hosted(&ApprovedRecipients::Any)
                .authorize_mpp(&split_charge(PAYEE, SPLIT), url)
                .is_ok()
        );
    }

    #[test]
    fn hosted_recipient_allowlist_covers_every_scheme() {
        let x402 = x402::Challenge {
            x402_version: 2,
            requirements: serde_json::from_value(serde_json::json!({
                "network": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
                "recipient": SPLIT,
                "amount": "1000",
                "currency": USDC,
                "resource": "https://merchant.example/paid",
            }))
            .unwrap(),
            siwx: None,
            extensions: None,
        };
        let url = "https://merchant.example/paid";
        assert!(only(&[PAYEE]).authorize_x402(&x402, url).is_err());
        assert!(only(&[SPLIT]).authorize_x402(&x402, url).is_ok());
        let batch = batch_challenge("1000");
        assert!(only(&[SPLIT]).authorize_batch(&batch, url).is_err());
        assert!(only(&[PAYEE]).authorize_batch(&batch, url).is_ok());
    }

    #[test]
    fn yaml_recipients_restrict_payment_destinations() {
        let config: PermissionConfig =
            serde_yml::from_str(&format!("recipients: [{PAYEE}]\nunlimited_amount: true\n"))
                .unwrap();
        let permissions = McpPermissions::from_config(&config).unwrap();
        let url = "https://merchant.example/paid";
        assert!(
            permissions
                .authorize_mpp(&split_charge(PAYEE, SPLIT), url)
                .is_err()
        );
        assert!(
            permissions
                .authorize_mpp(&split_charge(PAYEE, PAYEE), url)
                .is_ok()
        );
        assert!(
            serde_yml::from_str::<PermissionConfig>("recipients: []")
                .map(|config| config.build())
                .unwrap()
                .is_err()
        );
        assert!(
            serde_yml::from_str::<PermissionConfig>("recipients: [not-a-key]")
                .map(|config| config.build())
                .unwrap()
                .is_err()
        );
    }

    fn session_challenge(currency: &str, suggested: &str) -> mpp::Challenge {
        let request = serde_json::json!({
            "amount": "1",
            "currency": currency,
            "recipient": "11111111111111111111111111111111",
            "suggestedDeposit": suggested,
            "methodDetails": {
                "network": "mainnet",
                "channelProgram": "11111111111111111111111111111111",
                "voucherSigner": "operator",
            },
        });
        pay_kit::mpp::PaymentChallenge::new(
            "id",
            "realm",
            "solana",
            "session",
            pay_kit::mpp::Base64UrlJson::from_value(&request).unwrap(),
        )
    }

    #[test]
    fn session_deposit_is_authorized_against_the_cap() {
        let capped = McpPermissions::from_config(&PermissionConfig {
            max_payment: Some("$1".to_string()),
            ..Default::default()
        })
        .unwrap();
        assert!(
            capped
                .authorize_session(&session_challenge("USDC", "1000000"), "https://a.example/x")
                .is_ok()
        );
        assert!(
            capped
                .authorize_session(&session_challenge("USDC", "1000001"), "https://a.example/x")
                .is_err()
        );
        assert!(
            McpPermissions::hosted(&ApprovedRecipients::Any)
                .authorize_session(&session_challenge("SOL", "1"), "https://a.example/x")
                .is_err()
        );
    }

    /// A mainnet USDC operator session paying `payee`, with one 25% split.
    fn split_session(payee: &str, split: &str) -> mpp::Challenge {
        let request = serde_json::json!({
            "amount": "1",
            "currency": "USDC",
            "recipient": payee,
            "suggestedDeposit": "1000000",
            "methodDetails": {
                "network": "mainnet",
                "channelProgram": "11111111111111111111111111111111",
                "voucherSigner": "operator",
                "distributionSplits": [{ "recipient": split, "shareBps": 2500 }],
            },
        });
        pay_kit::mpp::PaymentChallenge::new(
            "id",
            "realm",
            "solana",
            "session",
            pay_kit::mpp::Base64UrlJson::from_value(&request).unwrap(),
        )
    }

    #[test]
    fn hosted_sessions_bind_the_payee_and_every_distribution_split() {
        let url = "https://a.example/x";
        let challenge = split_session(PAYEE, SPLIT);
        let error = only(&[PAYEE])
            .authorize_session(&challenge, url)
            .unwrap_err();
        assert!(error.to_string().contains(SPLIT), "{error}");
        assert!(only(&[SPLIT]).authorize_session(&challenge, url).is_err());

        // Control: approving both returns the payout the opener is held to.
        let payout = only(&[PAYEE, SPLIT])
            .authorize_session(&challenge, url)
            .unwrap();
        assert_eq!(payout.payee.to_string(), PAYEE);
        assert_eq!(payout.splits.len(), 1);
        assert_eq!(payout.splits[0].recipient.to_string(), SPLIT);
        assert_eq!(payout.splits[0].bps, 2500);
    }

    #[test]
    fn session_destinations_share_the_deposit_like_the_channel() {
        let payout = SessionPayout {
            payee: PAYEE.parse().unwrap(),
            splits: vec![pay_kit::mpp::program::payment_channels::Distribution {
                recipient: SPLIT.parse().unwrap(),
                bps: 2500,
            }],
        };
        let destinations = session_destinations(&payout, 1_000_001).unwrap();
        assert_eq!(destinations[0].amount, 750_001);
        assert_eq!(destinations[1].amount, 250_000);
        let mut over = payout.clone();
        over.splits[0].bps = 10_001;
        assert!(session_destinations(&over, 1_000).is_err());
    }

    fn batch_challenge(min_deposit: &str) -> x402::BatchChallenge {
        use base64::Engine;
        let envelope = serde_json::json!({
            "x402Version": 2,
            "accepts": [{
                "scheme": "batch-settlement",
                "network": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
                "amount": "1000",
                "asset": USDC,
                "payTo": "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY",
                "maxTimeoutSeconds": 300,
                "extra": {
                    "feePayer": "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
                    "withdrawDelay": 3600,
                    "minDeposit": min_deposit,
                    "tokenProgram": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
                }
            }]
        });
        let headers = vec![(
            "payment-required".to_string(),
            base64::engine::general_purpose::STANDARD
                .encode(serde_json::to_vec(&envelope).unwrap()),
        )];
        match pay_core::client::runner::classify_402(&headers, None, "https://a.example/x") {
            pay_core::client::runner::RunOutcome::X402BatchChallenge { challenge, .. } => {
                *challenge
            }
            other => panic!("expected batch challenge, got {other:?}"),
        }
    }

    #[test]
    fn caip2_networks_resolve_to_their_cluster() {
        assert_eq!(
            parse_network("solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp"),
            Ok(SolanaNetwork::Mainnet)
        );
        assert_eq!(
            parse_network("solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1"),
            Ok(SolanaNetwork::Devnet)
        );
        assert!(parse_network("solana:unknown").is_err());
    }

    #[test]
    fn batch_escrow_is_capped_even_when_the_request_price_fits() {
        let capped = McpPermissions::from_config(&PermissionConfig {
            max_payment: Some("$1".to_string()),
            ..Default::default()
        })
        .unwrap();
        let challenge = batch_challenge("5000000");
        assert!(
            capped
                .authorize_batch(&challenge, "https://a.example/x")
                .is_ok()
        );
        assert!(
            capped
                .authorize_batch_escrow(&challenge, "https://a.example/x", 5_000_000)
                .is_err()
        );
        assert!(
            capped
                .authorize_batch_escrow(&challenge, "https://a.example/x", 1_000_000)
                .is_ok()
        );
        assert!(
            capped
                .authorize_batch_escrow(&challenge, "https://a.example/x", 1_000_001)
                .is_err()
        );
    }
}
