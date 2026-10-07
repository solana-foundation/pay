//! Deployment-owned selling prices, separate from provider-cost metering.
//!
//! Only trusted adapters may supply deployment and wallet evidence. Neither the
//! public API consumer's identity nor application response headers are evidence
//! of the seller's ownership.

use std::collections::BTreeSet;

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::driver::{ComputeError, Result};

/// An owner-scoped wallet reference; never a private key or a caller-selected
/// settlement address.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct WalletReference {
    pub driver: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PaymentSplit {
    pub recipient: WalletReference,
    /// Hundredths of one percent. The primary recipient receives the remainder.
    pub basis_points: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PaymentPolicySpec {
    /// Integer millionths of USD: $0.05 is 50_000.
    pub price_micro_usd: u64,
    pub schemes: Vec<String>,
    pub primary_recipient: WalletReference,
    pub splits: Vec<PaymentSplit>,
    /// Unix seconds. Resolution fails closed at this time.
    pub expires_at: u64,
}

/// Trusted compute-provider identity, including the deployment incarnation so
/// deleting and recreating a deployment cannot silently resurrect its policy.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeploymentIdentity {
    pub owner_key: String,
    pub resource_name: String,
    pub created_at: String,
    pub hostname: String,
}

/// Evidence returned by the authenticated wallet service, not request JSON.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedWallet {
    pub owner_key: String,
    pub reference: WalletReference,
    pub chain: String,
    pub address: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PaymentPolicy {
    pub deployment: DeploymentIdentity,
    pub version: u64,
    /// A retained tombstone keeps revisions monotonic across delete/recreate.
    #[serde(default)]
    pub deleted: bool,
    pub spec: PaymentPolicySpec,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Allocation {
    pub recipient: String,
    pub amount_micro_usd: u64,
    /// Original payout weight, not reconstructed from rounded quote amounts.
    pub basis_points: u16,
}

impl PaymentPolicySpec {
    pub fn validate(&self, now: u64) -> Result<()> {
        if self.price_micro_usd == 0 {
            return Err(invalid("selling price must be positive"));
        }
        if self.schemes != ["mpp-session"] {
            return Err(invalid("only mpp-session is supported"));
        }
        if self.expires_at <= now {
            return Err(invalid("payment policy has expired"));
        }
        if self.splits.len() > 7 {
            return Err(invalid("at most eight total recipients are supported"));
        }
        let mut recipients = BTreeSet::new();
        validate_wallet_reference(&self.primary_recipient)?;
        recipients.insert(&self.primary_recipient);
        let mut total = 0u32;
        for split in &self.splits {
            validate_wallet_reference(&split.recipient)?;
            if split.basis_points == 0 || split.basis_points > 10_000 {
                return Err(invalid("split must be between 1 and 10000 basis points"));
            }
            if !recipients.insert(&split.recipient) {
                return Err(invalid("payment recipients must be unique"));
            }
            total = total
                .checked_add(u32::from(split.basis_points))
                .ok_or_else(|| invalid("split total overflow"))?;
            if total >= 10_000 {
                return Err(invalid("splits must leave a positive primary remainder"));
            }
        }
        Ok(())
    }
}

impl PaymentPolicy {
    /// Validate against current provider metadata, not a consumer-supplied payer.
    pub fn resolve(
        &self,
        deployment: &DeploymentIdentity,
        wallets: &[OwnedWallet],
        now: u64,
    ) -> Result<Vec<Allocation>> {
        if self.deleted || self.version == 0 || &self.deployment != deployment {
            return Err(invalid("payment policy does not match this deployment"));
        }
        self.spec.validate(now)?;
        let references = std::iter::once(&self.spec.primary_recipient)
            .chain(self.spec.splits.iter().map(|split| &split.recipient));
        let mut addresses = BTreeSet::new();
        let mut resolved = Vec::new();
        for reference in references {
            let mut matches = wallets.iter().filter(|wallet| {
                wallet.owner_key == deployment.owner_key && &wallet.reference == reference
            });
            let wallet = matches
                .next()
                .ok_or_else(|| invalid("recipient wallet is not owned by the deployment owner"))?;
            if matches.next().is_some() {
                return Err(invalid("ambiguous recipient wallet evidence"));
            }
            if wallet.chain != "solana"
                || bs58::decode(&wallet.address)
                    .into_vec()
                    .map_or(true, |address| address.len() != 32)
            {
                return Err(invalid("recipient must be a Solana wallet"));
            }
            if !addresses.insert(&wallet.address) {
                return Err(invalid("recipient addresses must be unique"));
            }
            resolved.push(wallet.address.clone());
        }
        let mut remainder = self.spec.price_micro_usd;
        let mut allocations = Vec::new();
        for (split, address) in self.spec.splits.iter().zip(resolved.iter().skip(1)) {
            // Widen before multiplication; floor each split and leave dust with
            // the primary recipient so allocations exactly conserve the price.
            let amount = (u128::from(self.spec.price_micro_usd) * u128::from(split.basis_points)
                / 10_000) as u64;
            if amount == 0 {
                return Err(invalid("split rounds to zero at this selling price"));
            }
            remainder -= amount;
            allocations.push(Allocation {
                recipient: address.clone(),
                amount_micro_usd: amount,
                basis_points: split.basis_points,
            });
        }
        allocations.insert(
            0,
            Allocation {
                recipient: resolved[0].clone(),
                amount_micro_usd: remainder,
                basis_points: 10_000
                    - self
                        .spec
                        .splits
                        .iter()
                        .map(|split| split.basis_points)
                        .sum::<u16>(),
            },
        );
        Ok(allocations)
    }
}

/// Compute the next revision. The storage adapter must atomically compare its
/// read revision when persisting this result. Exact retries are no-ops; stale
/// writers cannot replace a newer, different policy.
pub fn revision(
    current: Option<&PaymentPolicy>,
    deployment: DeploymentIdentity,
    spec: PaymentPolicySpec,
    expected_version: u64,
    now: u64,
) -> Result<PaymentPolicy> {
    spec.validate(now)?;
    if deployment.owner_key.is_empty()
        || deployment.resource_name.is_empty()
        || deployment.created_at.is_empty()
        || deployment.hostname.is_empty()
    {
        return Err(invalid("verified deployment identity is incomplete"));
    }
    match current {
        Some(current) => {
            if current.deployment != deployment {
                return Err(invalid("payment policy belongs to a different deployment"));
            }
            if current.version == 0 {
                return Err(invalid("stored payment policy version is invalid"));
            }
            if !current.deleted && current.spec == spec {
                return Ok(current.clone());
            }
            if expected_version != current.version {
                return Err(invalid("payment policy version conflict"));
            }
        }
        None if expected_version != 0 => {
            return Err(invalid("payment policy version conflict"));
        }
        None => {}
    }
    let version = current
        .map_or(0, |policy| policy.version)
        .checked_add(1)
        .ok_or_else(|| invalid("payment policy version exhausted"))?;
    Ok(PaymentPolicy {
        deployment,
        version,
        deleted: false,
        spec,
    })
}

fn validate_wallet_reference(reference: &WalletReference) -> Result<()> {
    if reference.driver != "privy"
        || reference.name.is_empty()
        || reference.name.len() > 40
        || !reference.name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(invalid("invalid owner-scoped Privy wallet reference"));
    }
    Ok(())
}

fn invalid(message: &str) -> ComputeError {
    ComputeError::InvalidRequest(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (DeploymentIdentity, PaymentPolicySpec, Vec<OwnedWallet>) {
        let deployment = DeploymentIdentity {
            owner_key: "owner-a".into(),
            resource_name: "projects/p/locations/r/functions/gcf-owner-a-sec".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            hostname: "gcf-owner-a-sec.compute.example".into(),
        };
        let wallets: Vec<_> = ["infrastructure", "tax", "profit"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| OwnedWallet {
                owner_key: deployment.owner_key.clone(),
                reference: WalletReference {
                    driver: "privy".into(),
                    name: name.into(),
                },
                chain: "solana".into(),
                address: bs58::encode([index as u8 + 1; 32]).into_string(),
            })
            .collect();
        let spec = PaymentPolicySpec {
            price_micro_usd: 50_000,
            schemes: vec!["mpp-session".into()],
            primary_recipient: wallets[0].reference.clone(),
            splits: vec![
                PaymentSplit {
                    recipient: wallets[1].reference.clone(),
                    basis_points: 3_000,
                },
                PaymentSplit {
                    recipient: wallets[2].reference.clone(),
                    basis_points: 1_000,
                },
            ],
            expires_at: 100,
        };
        (deployment, spec, wallets)
    }

    #[test]
    fn five_cents_allocates_thirty_ten_sixty() {
        let (deployment, spec, wallets) = fixture();
        let policy = revision(None, deployment.clone(), spec, 0, 1).unwrap();
        let allocation = policy.resolve(&deployment, &wallets, 1).unwrap();
        assert_eq!(
            allocation
                .iter()
                .map(|a| a.amount_micro_usd)
                .collect::<Vec<_>>(),
            [30_000, 15_000, 5_000]
        );
        assert_eq!(
            allocation.iter().map(|a| a.amount_micro_usd).sum::<u64>(),
            50_000
        );
        assert_eq!(
            allocation
                .iter()
                .map(|a| a.basis_points)
                .collect::<Vec<_>>(),
            [6_000, 3_000, 1_000]
        );
    }

    #[test]
    fn rounded_quotes_preserve_original_payout_weights() {
        let (deployment, mut spec, wallets) = fixture();
        spec.price_micro_usd = 50_003;
        let policy = revision(None, deployment.clone(), spec, 0, 1).unwrap();
        let allocations = policy.resolve(&deployment, &wallets, 1).unwrap();
        assert_eq!(
            allocations
                .iter()
                .map(|a| a.amount_micro_usd)
                .collect::<Vec<_>>(),
            [30_003, 15_000, 5_000]
        );
        assert_eq!(
            allocations
                .iter()
                .map(|a| a.basis_points)
                .collect::<Vec<_>>(),
            [6_000, 3_000, 1_000]
        );
    }

    #[test]
    fn rejects_cross_tenant_wallets_and_deployments() {
        let (deployment, spec, mut wallets) = fixture();
        let policy = revision(None, deployment.clone(), spec, 0, 1).unwrap();
        wallets[1].owner_key = "owner-b".into();
        assert!(policy.resolve(&deployment, &wallets, 1).is_err());
        let mut other = deployment.clone();
        other.owner_key = "owner-b".into();
        assert!(policy.resolve(&other, &wallets, 1).is_err());
        other = deployment.clone();
        other.hostname = "different.compute.example".into();
        assert!(policy.resolve(&other, &wallets, 1).is_err());
        other = deployment;
        other.created_at = "2026-01-02T00:00:00Z".into();
        assert!(policy.resolve(&other, &wallets, 1).is_err());
    }

    #[test]
    fn updates_are_versioned_idempotent_and_conflict_checked() {
        let (deployment, spec, _) = fixture();
        let first = revision(None, deployment.clone(), spec.clone(), 0, 1).unwrap();
        assert_eq!(first.version, 1);
        assert_eq!(
            revision(Some(&first), deployment.clone(), spec.clone(), 0, 1).unwrap(),
            first
        );
        let mut changed = spec;
        changed.price_micro_usd += 1;
        assert!(revision(Some(&first), deployment.clone(), changed.clone(), 0, 1).is_err());
        let second = revision(Some(&first), deployment, changed, 1, 1).unwrap();
        assert_eq!(second.version, 2);
    }

    #[test]
    fn rejects_invalid_splits_prices_schemes_and_expiry() {
        let (_, spec, _) = fixture();
        for total in [10_000, 10_001] {
            let mut invalid = spec.clone();
            invalid.splits[0].basis_points = total - 1_000;
            assert!(invalid.validate(1).is_err());
        }
        let mut invalid = spec.clone();
        invalid.splits[0].basis_points = 0;
        assert!(invalid.validate(1).is_err());
        invalid = spec.clone();
        invalid.splits[0].recipient = invalid.primary_recipient.clone();
        assert!(invalid.validate(1).is_err());
        invalid = spec.clone();
        invalid.price_micro_usd = 0;
        assert!(invalid.validate(1).is_err());
        invalid = spec.clone();
        invalid.schemes.push("x402".into());
        assert!(invalid.validate(1).is_err());
        assert!(spec.validate(100).is_err());
    }

    #[test]
    fn rounding_conserves_value_without_overflow() {
        let (deployment, mut spec, wallets) = fixture();
        for price in [50_003, u64::MAX] {
            spec.price_micro_usd = price;
            let policy = revision(None, deployment.clone(), spec.clone(), 0, 1).unwrap();
            let allocation = policy.resolve(&deployment, &wallets, 1).unwrap();
            assert_eq!(
                allocation
                    .iter()
                    .map(|a| u128::from(a.amount_micro_usd))
                    .sum::<u128>(),
                u128::from(price)
            );
        }
    }
}
