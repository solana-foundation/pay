//! Immutable application identity carried by a deployment-funded session.
//!
//! Expiry and deployment availability govern admission, never payout recovery.
use std::collections::HashSet;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;

/// A malformed durable binding is not a legacy channel.
#[derive(Debug, thiserror::Error)]
pub enum BindingError {
    #[error("invalid deployment session binding: {0}")]
    Invalid(&'static str),
    #[error("malformed deployment session binding")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentIdentity {
    pub owner_key: String,
    pub resource_name: String,
    pub created_at: String,
    pub hostname: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyAllocation {
    pub recipient: String,
    pub amount_micro_usd: u64,
    pub basis_points: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableDeploymentPolicy {
    pub deployment: DeploymentIdentity,
    pub version: u64,
    /// Six-decimal USDC base units; USD micro-units are identical.
    pub price_micro_usd: u64,
    /// Primary recipient first, followed by ordered splits.
    pub allocations: Vec<PolicyAllocation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentSessionBinding {
    pub schema_version: u32,
    pub policy: DurableDeploymentPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayoutSplit {
    pub recipient: String,
    pub bps: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectivePayout {
    pub recipient: String,
    pub splits: Vec<PayoutSplit>,
}

const LIFECYCLE_KEY: &str = "payDeploymentLifecycle";

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LifecycleClaims {
    version: u32,
    reservation: Option<Reservation>,
    close: Option<CloseClaim>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reservation {
    id: String,
    amount: u64,
    expires_at_ms: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseClaim {
    owner: String,
    expires_at_ms: u64,
}

fn claims(state: &pay_kit::core::store::ChannelState) -> Result<LifecycleClaims, BindingError> {
    match state.extra.get(LIFECYCLE_KEY) {
        None => Ok(LifecycleClaims {
            version: 1,
            ..Default::default()
        }),
        Some(value) => {
            let claims: LifecycleClaims = serde_json::from_value(value.clone())?;
            if claims.version != 1 {
                return Err(BindingError::Invalid("unsupported lifecycle version"));
            }
            Ok(claims)
        }
    }
}

fn write_claims(
    state: &mut pay_kit::core::store::ChannelState,
    claims: LifecycleClaims,
) -> Result<(), BindingError> {
    state
        .extra
        .insert(LIFECYCLE_KEY.into(), serde_json::to_value(claims)?);
    Ok(())
}

/// Apply inside the store's atomic update, never to a scanned copy.
pub fn reserve(
    state: &mut pay_kit::core::store::ChannelState,
    lease_id: &str,
    amount: u64,
    now_ms: u64,
    expires_at_ms: u64,
) -> Result<(), BindingError> {
    let mut claims = claims(state)?;
    if lease_id.is_empty()
        || expires_at_ms <= now_ms
        || state.sealed
        || state.close_requested_at.is_some()
        || state.pending_setup.is_some()
        || claims.close.is_some()
        || claims
            .reservation
            .as_ref()
            .is_some_and(|r| r.expires_at_ms > now_ms)
        || amount > state.deposit.saturating_sub(state.cumulative)
    {
        return Err(BindingError::Invalid("channel cannot reserve capacity"));
    }
    claims.reservation = Some(Reservation {
        id: lease_id.into(),
        amount,
        expires_at_ms,
    });
    write_claims(state, claims)
}

pub fn require_reservation(
    state: &pay_kit::core::store::ChannelState,
    lease_id: &str,
    amount: u64,
    now_ms: u64,
) -> Result<(), BindingError> {
    let claims = claims(state)?;
    if state.sealed
        || state.close_requested_at.is_some()
        || claims.close.is_some()
        || !claims
            .reservation
            .as_ref()
            .is_some_and(|r| r.id == lease_id && r.amount >= amount && r.expires_at_ms > now_ms)
    {
        return Err(BindingError::Invalid(
            "reservation expired or channel closing",
        ));
    }
    Ok(())
}

pub fn renew(
    state: &mut pay_kit::core::store::ChannelState,
    lease_id: &str,
    now_ms: u64,
    expires_at_ms: u64,
) -> Result<(), BindingError> {
    require_reservation(state, lease_id, 0, now_ms)?;
    if expires_at_ms <= now_ms {
        return Err(BindingError::Invalid("invalid reservation deadline"));
    }
    let mut claims = claims(state)?;
    claims
        .reservation
        .as_mut()
        .expect("checked reservation")
        .expires_at_ms = expires_at_ms;
    write_claims(state, claims)
}

/// Releasing a stale request never clears a replacement request's reservation.
pub fn release(
    state: &mut pay_kit::core::store::ChannelState,
    lease_id: &str,
) -> Result<(), BindingError> {
    let mut claims = claims(state)?;
    if claims
        .reservation
        .as_ref()
        .is_some_and(|r| r.id == lease_id)
    {
        claims.reservation = None;
        write_claims(state, claims)?;
    }
    Ok(())
}

/// Single durable close transition for bound channels. The store CAS serializes
/// this with both reservation acquisition and guarded voucher commits.
/// Expired ownership can be reclaimed after a crash, but closing is permanent.
pub fn claim_close(
    state: &mut pay_kit::core::store::ChannelState,
    owner: &str,
    now_ms: u64,
    expires_at_ms: u64,
) -> Result<bool, BindingError> {
    let mut claims = claims(state)?;
    if owner.is_empty() || expires_at_ms <= now_ms {
        return Err(BindingError::Invalid("invalid close lease"));
    }
    if state.sealed
        || state.open_slot.is_none()
        || state.pending_setup.is_some()
        || state.has_blocking_authorization((now_ms / 1000) as i64)
        || claims
            .reservation
            .as_ref()
            .is_some_and(|r| r.expires_at_ms > now_ms)
        || claims
            .close
            .as_ref()
            .is_some_and(|c| c.owner != owner && c.expires_at_ms > now_ms)
        || (state.close_requested_at.is_none()
            && state
                .lifecycle
                .as_ref()
                .is_none_or(|l| l.close_after > now_ms))
    {
        return Ok(false);
    }
    claims.reservation = None;
    claims.close = Some(CloseClaim {
        owner: owner.into(),
        expires_at_ms,
    });
    state.close_requested_at.get_or_insert(now_ms / 1000);
    write_claims(state, claims)?;
    Ok(true)
}

impl DeploymentSessionBinding {
    pub fn new(policy: DurableDeploymentPolicy) -> Result<Self, BindingError> {
        let binding = Self {
            schema_version: 1,
            policy,
        };
        binding.validate()?;
        Ok(binding)
    }

    pub fn from_value(value: serde_json::Value) -> Result<Self, BindingError> {
        let binding: Self = serde_json::from_value(value)?;
        binding.validate()?;
        Ok(binding)
    }

    pub fn to_value(&self) -> Result<serde_json::Value, BindingError> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn validate(&self) -> Result<(), BindingError> {
        if self.schema_version != 1 {
            return Err(BindingError::Invalid("unsupported schema version"));
        }
        let policy = &self.policy;
        let identity = &policy.deployment;
        if [
            &identity.owner_key,
            &identity.resource_name,
            &identity.created_at,
            &identity.hostname,
        ]
        .iter()
        .any(|value| value.is_empty() || value.trim() != value.as_str())
            || policy.version == 0
            || policy.price_micro_usd == 0
            || policy.allocations.is_empty()
            || policy.allocations.len() > 8
        {
            return Err(BindingError::Invalid("missing identity or monetary terms"));
        }
        let mut recipients = HashSet::new();
        let mut amount = 0_u64;
        let mut bps = 0_u32;
        let mut split_amount = 0_u64;
        for (index, allocation) in policy.allocations.iter().enumerate() {
            if Pubkey::from_str(&allocation.recipient).is_err()
                || allocation.recipient == Pubkey::default().to_string()
                || !recipients.insert(&allocation.recipient)
                || allocation.amount_micro_usd == 0
                || allocation.basis_points == 0
            {
                return Err(BindingError::Invalid("invalid allocation"));
            }
            amount = amount
                .checked_add(allocation.amount_micro_usd)
                .ok_or(BindingError::Invalid("allocation overflow"))?;
            bps += u32::from(allocation.basis_points);
            if index > 0 {
                let expected = u128::from(policy.price_micro_usd)
                    * u128::from(allocation.basis_points)
                    / 10_000;
                if u128::from(allocation.amount_micro_usd) != expected {
                    return Err(BindingError::Invalid(
                        "split amount differs from basis points",
                    ));
                }
                split_amount = split_amount
                    .checked_add(allocation.amount_micro_usd)
                    .ok_or(BindingError::Invalid("allocation overflow"))?;
            }
        }
        if amount != policy.price_micro_usd
            || bps != 10_000
            || policy.price_micro_usd.checked_sub(split_amount)
                != Some(policy.allocations[0].amount_micro_usd)
        {
            return Err(BindingError::Invalid("allocation totals differ from price"));
        }
        Ok(())
    }

    /// Match the delegated-session conversion: operator owns the channel while
    /// the primary seller's remainder becomes the final ordered split.
    pub fn effective_payout(&self, operator: &str) -> Result<EffectivePayout, BindingError> {
        self.validate()?;
        Pubkey::from_str(operator).map_err(|_| BindingError::Invalid("invalid operator"))?;
        let primary = &self.policy.allocations[0];
        if primary.recipient != operator
            && u128::from(self.policy.price_micro_usd) * u128::from(primary.basis_points) / 10_000
                != u128::from(primary.amount_micro_usd)
        {
            return Err(BindingError::Invalid(
                "delegation would redirect primary rounding remainder",
            ));
        }
        let mut splits = self.policy.allocations[1..]
            .iter()
            .map(|allocation| PayoutSplit {
                recipient: allocation.recipient.clone(),
                bps: allocation.basis_points,
            })
            .collect::<Vec<_>>();
        if primary.recipient != operator {
            splits.push(PayoutSplit {
                recipient: primary.recipient.clone(),
                bps: primary.basis_points,
            });
        }
        Ok(EffectivePayout {
            recipient: operator.to_owned(),
            splits,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> pay_kit::core::store::ChannelState {
        serde_json::from_value(serde_json::json!({
            "channel_id":"channel", "authorized_signer":"signer",
            "deposit":100, "cumulative":0, "sealed":false,
            "payer":"payer", "rent_payer":"payer", "open_slot":42,
            "lifecycle":{"owner":"proxy","closeAfter":100}
        }))
        .unwrap()
    }

    #[test]
    fn reservation_and_close_share_one_transition_and_expired_owner_is_recoverable() {
        let mut state = state();
        reserve(&mut state, "request", 50, 100, 200).unwrap();
        assert!(!claim_close(&mut state, "worker-a", 150, 250).unwrap());
        assert!(require_reservation(&state, "request", 50, 150).is_ok());
        assert!(claim_close(&mut state, "worker-a", 200, 300).unwrap());
        assert!(require_reservation(&state, "request", 50, 200).is_err());
        assert!(reserve(&mut state, "new-request", 1, 200, 400).is_err());
        assert!(!claim_close(&mut state, "worker-b", 250, 350).unwrap());
        assert!(claim_close(&mut state, "worker-b", 300, 400).unwrap());
        assert!(!claim_close(&mut state, "worker-a", 301, 401).unwrap());
    }

    #[test]
    fn stale_release_preserves_new_reservation() {
        let mut state = state();
        reserve(&mut state, "old", 10, 1, 2).unwrap();
        reserve(&mut state, "new", 20, 2, 100).unwrap();
        release(&mut state, "old").unwrap();
        require_reservation(&state, "new", 20, 3).unwrap();
    }

    fn binding() -> DeploymentSessionBinding {
        DeploymentSessionBinding::new(DurableDeploymentPolicy {
            deployment: DeploymentIdentity {
                owner_key: "owner".into(),
                resource_name: "deployment".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                hostname: "app.example.com".into(),
            },
            version: 1,
            price_micro_usd: 50_000,
            allocations: [6000, 3000, 1000]
                .into_iter()
                .map(|basis_points| PolicyAllocation {
                    recipient: Pubkey::new_unique().to_string(),
                    amount_micro_usd: u64::from(basis_points) * 5,
                    basis_points,
                })
                .collect(),
        })
        .unwrap()
    }

    #[test]
    fn exact_ordered_payout_survives_serialization_without_live_policy() {
        let binding = binding();
        let restored = DeploymentSessionBinding::from_value(binding.to_value().unwrap()).unwrap();
        let payout = restored
            .effective_payout(&Pubkey::new_unique().to_string())
            .unwrap();
        assert_eq!(
            payout.splits.iter().map(|s| s.bps).collect::<Vec<_>>(),
            [3000, 1000, 6000]
        );
        assert_eq!(
            payout.splits[2].recipient,
            binding.policy.allocations[0].recipient
        );
    }

    #[test]
    fn unknown_versions_fields_and_inexact_amounts_fail_closed() {
        let mut value = binding().to_value().unwrap();
        value["schema_version"] = 2.into();
        assert!(DeploymentSessionBinding::from_value(value).is_err());
        let mut value = binding().to_value().unwrap();
        value["extra"] = true.into();
        assert!(DeploymentSessionBinding::from_value(value).is_err());
        let mut binding = binding();
        binding.policy.allocations[0].amount_micro_usd += 1;
        assert!(binding.validate().is_err());
    }
}
