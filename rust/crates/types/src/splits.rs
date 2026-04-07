//! Split resolution — converts `SplitRule`s into concrete amounts for on-chain transfers.
//!
//! **Semantics:**
//! - `amount`: fixed USD value, deducted from the charge as-is.
//! - `percent`: percentage of the **original total charge** (not the remaining balance).
//!
//! Both types always reference the original total. This means reordering split rules
//! does not change anyone's payout — the standard payment processing model.
//!
//! **Validation:**
//! - Each rule must have exactly one of `amount` or `percent`.
//! - The `recipient` alias must exist in the `recipients` map.
//! - Runtime accounts (`${VAR}`) are resolved from request query parameters.
//! - The sum of all resolved splits must be strictly less than the total charge
//!   (the primary recipient must receive a positive amount).

use std::collections::HashMap;

use crate::metering::{RecipientAlias, SplitRule};

/// A fully resolved split ready for the MPP charge.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSplit {
    /// Base58 wallet account.
    pub recipient: String,
    /// Amount in token smallest units (e.g. USDC has 6 decimals → 1 USDC = 1_000_000).
    pub amount: u64,
    /// Human-readable label from the recipient alias (e.g. "Vendor", "Tax Authority").
    pub label: Option<String>,
    /// Human-readable memo.
    pub memo: Option<String>,
}

/// Errors from split resolution.
#[derive(Debug, Clone, PartialEq)]
pub enum SplitError {
    /// The `recipient` alias was not found in the `recipients` map.
    UnknownRecipient(String),
    /// A split rule has both `amount` and `percent` set.
    AmbiguousRule(String),
    /// A split rule has neither `amount` nor `percent` set.
    EmptyRule(String),
    /// The sum of all splits is >= the total charge.
    SplitsExceedTotal { total_usd: f64, splits_usd: f64 },
    /// A runtime account variable could not be resolved from query parameters.
    UnresolvableAccount { recipient: String, var_name: String },
}

impl std::fmt::Display for SplitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownRecipient(name) => write!(f, "Unknown recipient alias: '{name}'"),
            Self::AmbiguousRule(name) => {
                write!(
                    f,
                    "Split for '{name}' has both amount and percent — pick one"
                )
            }
            Self::EmptyRule(name) => {
                write!(f, "Split for '{name}' has neither amount nor percent")
            }
            Self::SplitsExceedTotal {
                total_usd,
                splits_usd,
            } => {
                write!(
                    f,
                    "Splits total ({splits_usd:.6} USD) >= charge total ({total_usd:.6} USD) — primary recipient must receive a positive amount"
                )
            }
            Self::UnresolvableAccount {
                recipient,
                var_name,
            } => {
                write!(
                    f,
                    "Runtime account for '{recipient}' requires query parameter '{var_name}' but it was not provided"
                )
            }
        }
    }
}

impl std::error::Error for SplitError {}

/// Resolve split rules into concrete token amounts.
///
/// # Arguments
/// - `rules` — split directives from the YAML spec (metering or tier level)
/// - `recipients` — named aliases from `ApiSpec.recipients`
/// - `total_usd` — the total charge amount in USD
/// - `decimals` — token decimals (6 for USDC, 9 for SOL)
/// - `query_params` — request query parameters for runtime account resolution
///
/// # Returns
/// A vec of `ResolvedSplit` with concrete base58 accounts and token amounts.
///
/// # TODO
/// - Validate that payer ≠ any split recipient. Currently we don't have the payer's
///   pubkey at resolution time. This is a naive check — proper validation requires
///   comparing actual on-chain pubkeys, not string equality on aliases.
pub fn resolve_splits(
    rules: &[SplitRule],
    recipients: &HashMap<String, RecipientAlias>,
    total_usd: f64,
    decimals: u8,
    query_params: &HashMap<String, String>,
) -> Result<Vec<ResolvedSplit>, SplitError> {
    if rules.is_empty() {
        return Ok(vec![]);
    }

    let divisor = 10f64.powi(decimals as i32);
    let mut resolved = Vec::with_capacity(rules.len());
    let mut splits_total_usd = 0.0;

    for rule in rules {
        // Look up recipient alias.
        let alias = recipients
            .get(&rule.recipient)
            .ok_or_else(|| SplitError::UnknownRecipient(rule.recipient.clone()))?;

        // Validate exactly one of amount/percent.
        match (rule.amount, rule.percent) {
            (Some(_), Some(_)) => {
                return Err(SplitError::AmbiguousRule(rule.recipient.clone()));
            }
            (None, None) => {
                return Err(SplitError::EmptyRule(rule.recipient.clone()));
            }
            _ => {}
        }

        // Resolve account address (literal or runtime).
        let account = resolve_account(&alias.account, &rule.recipient, query_params)?;

        // Compute USD amount.
        let usd = if let Some(fixed) = rule.amount {
            fixed
        } else {
            total_usd * rule.percent.unwrap() / 100.0
        };

        splits_total_usd += usd;

        // Convert to token units.
        let token_amount = (usd * divisor).round() as u64;

        resolved.push(ResolvedSplit {
            recipient: account,
            amount: token_amount,
            label: alias.label.clone(),
            memo: rule.memo.clone(),
        });
    }

    // Validate sum < total.
    if splits_total_usd >= total_usd {
        return Err(SplitError::SplitsExceedTotal {
            total_usd,
            splits_usd: splits_total_usd,
        });
    }

    Ok(resolved)
}

/// Resolve an account string. If it starts with `${`, treat it as a runtime variable
/// and look it up in `query_params`.
///
/// Resolution strategy for `${SOME_WALLET}`:
/// 1. Try `some_wallet` (full lowercase)
/// 2. Try stripping `_wallet`/`_account` suffix → `some`
fn resolve_account(
    account: &str,
    recipient_name: &str,
    query_params: &HashMap<String, String>,
) -> Result<String, SplitError> {
    if !account.starts_with("${") || !account.ends_with('}') {
        return Ok(account.to_string());
    }

    let var_name = &account[2..account.len() - 1]; // e.g. "AFFILIATE_WALLET"
    let lower = var_name.to_lowercase(); // e.g. "affiliate_wallet"

    // Try exact lowercase match first.
    if let Some(val) = query_params.get(&lower) {
        return Ok(val.clone());
    }

    // Strip common suffixes and try again.
    for suffix in ["_wallet", "_account", "_address"] {
        if let Some(stripped) = lower.strip_suffix(suffix)
            && let Some(val) = query_params.get(stripped)
        {
            return Ok(val.clone());
        }
    }

    // Try the recipient alias name itself as the query param key.
    if let Some(val) = query_params.get(recipient_name) {
        return Ok(val.clone());
    }

    Err(SplitError::UnresolvableAccount {
        recipient: recipient_name.to_string(),
        var_name: lower,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipients() -> HashMap<String, RecipientAlias> {
        let mut m = HashMap::new();
        m.insert(
            "vendor".into(),
            RecipientAlias {
                account: "VendorWaLLetxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx".into(),
                label: Some("Vendor".into()),
            },
        );
        m.insert(
            "platform".into(),
            RecipientAlias {
                account: "PlatformWaLLetxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx".into(),
                label: Some("Platform".into()),
            },
        );
        m.insert(
            "tax".into(),
            RecipientAlias {
                account: "TaxWaLLetxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx".into(),
                label: Some("Tax Authority".into()),
            },
        );
        m.insert(
            "affiliate".into(),
            RecipientAlias {
                account: "${AFFILIATE_WALLET}".into(),
                label: Some("Affiliate".into()),
            },
        );
        m
    }

    fn no_params() -> HashMap<String, String> {
        HashMap::new()
    }

    // ── Parsing / validation ──

    #[test]
    fn parse_fixed_amount_split() {
        let rules = vec![SplitRule {
            recipient: "vendor".into(),
            amount: Some(0.30),
            percent: None,
            memo: None,
        }];
        let result = resolve_splits(&rules, &recipients(), 1.0, 6, &no_params());
        assert!(result.is_ok());
        assert_eq!(result.unwrap()[0].amount, 300_000);
    }

    #[test]
    fn parse_percent_split() {
        let rules = vec![SplitRule {
            recipient: "platform".into(),
            amount: None,
            percent: Some(2.9),
            memo: None,
        }];
        let result = resolve_splits(&rules, &recipients(), 100.0, 6, &no_params());
        assert!(result.is_ok());
        assert_eq!(result.unwrap()[0].amount, 2_900_000); // 2.9% of 100 = 2.90
    }

    #[test]
    fn parse_split_with_memo() {
        let rules = vec![SplitRule {
            recipient: "vendor".into(),
            amount: Some(10.0),
            percent: None,
            memo: Some("Vendor payout".into()),
        }];
        let result = resolve_splits(&rules, &recipients(), 100.0, 6, &no_params()).unwrap();
        assert_eq!(result[0].memo.as_deref(), Some("Vendor payout"));
    }

    #[test]
    fn reject_both_amount_and_percent() {
        let rules = vec![SplitRule {
            recipient: "vendor".into(),
            amount: Some(1.0),
            percent: Some(5.0),
            memo: None,
        }];
        let err = resolve_splits(&rules, &recipients(), 10.0, 6, &no_params()).unwrap_err();
        assert!(matches!(err, SplitError::AmbiguousRule(_)));
    }

    #[test]
    fn reject_neither_amount_nor_percent() {
        let rules = vec![SplitRule {
            recipient: "vendor".into(),
            amount: None,
            percent: None,
            memo: None,
        }];
        let err = resolve_splits(&rules, &recipients(), 10.0, 6, &no_params()).unwrap_err();
        assert!(matches!(err, SplitError::EmptyRule(_)));
    }

    // ── Resolution ──

    #[test]
    fn resolve_fixed_splits() {
        let rules = vec![
            SplitRule {
                recipient: "vendor".into(),
                amount: Some(85.0),
                percent: None,
                memo: Some("Vendor payout".into()),
            },
            SplitRule {
                recipient: "platform".into(),
                amount: Some(2.90),
                percent: None,
                memo: Some("Processing fee".into()),
            },
        ];
        let result = resolve_splits(&rules, &recipients(), 100.0, 6, &no_params()).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].amount, 85_000_000);
        assert_eq!(result[1].amount, 2_900_000);
    }

    #[test]
    fn resolve_percent_splits() {
        let rules = vec![
            SplitRule {
                recipient: "platform".into(),
                amount: None,
                percent: Some(2.9),
                memo: None,
            },
            SplitRule {
                recipient: "tax".into(),
                amount: None,
                percent: Some(7.0),
                memo: None,
            },
        ];
        let result = resolve_splits(&rules, &recipients(), 49.99, 6, &no_params()).unwrap();
        // 2.9% of 49.99 = 1.44971
        assert_eq!(result[0].amount, 1_449_710);
        // 7% of 49.99 = 3.4993
        assert_eq!(result[1].amount, 3_499_300);
    }

    #[test]
    fn resolve_mixed_splits() {
        let rules = vec![
            SplitRule {
                recipient: "platform".into(),
                amount: Some(0.30),
                percent: None,
                memo: Some("Fixed fee".into()),
            },
            SplitRule {
                recipient: "platform".into(),
                amount: None,
                percent: Some(2.9),
                memo: Some("Variable fee".into()),
            },
            SplitRule {
                recipient: "tax".into(),
                amount: None,
                percent: Some(8.25),
                memo: Some("Sales tax".into()),
            },
        ];
        let result = resolve_splits(&rules, &recipients(), 250.0, 6, &no_params()).unwrap();
        assert_eq!(result[0].amount, 300_000); // $0.30
        assert_eq!(result[1].amount, 7_250_000); // 2.9% of 250 = $7.25
        assert_eq!(result[2].amount, 20_625_000); // 8.25% of 250 = $20.625
    }

    #[test]
    fn resolve_order_independent() {
        // Swapping two percentage splits should give the same amounts
        let rules_a = vec![
            SplitRule {
                recipient: "platform".into(),
                amount: None,
                percent: Some(5.0),
                memo: None,
            },
            SplitRule {
                recipient: "tax".into(),
                amount: None,
                percent: Some(10.0),
                memo: None,
            },
        ];
        let rules_b = vec![
            SplitRule {
                recipient: "tax".into(),
                amount: None,
                percent: Some(10.0),
                memo: None,
            },
            SplitRule {
                recipient: "platform".into(),
                amount: None,
                percent: Some(5.0),
                memo: None,
            },
        ];
        let a = resolve_splits(&rules_a, &recipients(), 100.0, 6, &no_params()).unwrap();
        let b = resolve_splits(&rules_b, &recipients(), 100.0, 6, &no_params()).unwrap();
        // Same amounts regardless of order (platform=5M, tax=10M)
        assert_eq!(a[0].amount, b[1].amount);
        assert_eq!(a[1].amount, b[0].amount);
    }

    #[test]
    fn resolve_sum_exceeds_total() {
        let rules = vec![SplitRule {
            recipient: "vendor".into(),
            amount: Some(101.0),
            percent: None,
            memo: None,
        }];
        let err = resolve_splits(&rules, &recipients(), 100.0, 6, &no_params()).unwrap_err();
        assert!(matches!(err, SplitError::SplitsExceedTotal { .. }));
    }

    #[test]
    fn resolve_sum_equals_total() {
        let rules = vec![SplitRule {
            recipient: "vendor".into(),
            amount: Some(100.0),
            percent: None,
            memo: None,
        }];
        let err = resolve_splits(&rules, &recipients(), 100.0, 6, &no_params()).unwrap_err();
        assert!(matches!(err, SplitError::SplitsExceedTotal { .. }));
    }

    #[test]
    fn resolve_empty_splits() {
        let result = resolve_splits(&[], &recipients(), 100.0, 6, &no_params()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn resolve_unknown_recipient() {
        let rules = vec![SplitRule {
            recipient: "nonexistent".into(),
            amount: Some(1.0),
            percent: None,
            memo: None,
        }];
        let err = resolve_splits(&rules, &recipients(), 10.0, 6, &no_params()).unwrap_err();
        assert!(matches!(err, SplitError::UnknownRecipient(_)));
    }

    // ── Runtime account resolution ──

    #[test]
    fn resolve_runtime_account() {
        let rules = vec![SplitRule {
            recipient: "affiliate".into(),
            amount: Some(1.0),
            percent: None,
            memo: None,
        }];
        let mut params = HashMap::new();
        params.insert(
            "affiliate_wallet".into(),
            "RuntimeAffiliateWaLLetxxxxxxxxxxxxxxxxxxx".into(),
        );
        let result = resolve_splits(&rules, &recipients(), 10.0, 6, &params).unwrap();
        assert_eq!(
            result[0].recipient,
            "RuntimeAffiliateWaLLetxxxxxxxxxxxxxxxxxxx"
        );
    }

    #[test]
    fn resolve_runtime_account_missing() {
        let rules = vec![SplitRule {
            recipient: "affiliate".into(),
            amount: Some(1.0),
            percent: None,
            memo: None,
        }];
        let err = resolve_splits(&rules, &recipients(), 10.0, 6, &no_params()).unwrap_err();
        assert!(matches!(err, SplitError::UnresolvableAccount { .. }));
    }

    #[test]
    fn resolve_runtime_account_from_stripped_key() {
        // ${AFFILIATE_WALLET} → try "affiliate" (stripped _wallet suffix)
        let rules = vec![SplitRule {
            recipient: "affiliate".into(),
            amount: Some(1.0),
            percent: None,
            memo: None,
        }];
        let mut params = HashMap::new();
        params.insert(
            "affiliate".into(),
            "StrippedKeyWaLLetxxxxxxxxxxxxxxxxxxxxxxxxx".into(),
        );
        let result = resolve_splits(&rules, &recipients(), 10.0, 6, &params).unwrap();
        assert_eq!(
            result[0].recipient,
            "StrippedKeyWaLLetxxxxxxxxxxxxxxxxxxxxxxxxx"
        );
    }

    #[test]
    fn resolve_runtime_account_from_alias_name() {
        // Falls back to recipient alias name as query param key
        let rules = vec![SplitRule {
            recipient: "affiliate".into(),
            amount: Some(1.0),
            percent: None,
            memo: None,
        }];
        let mut params = HashMap::new();
        params.insert(
            "affiliate".into(),
            "AliasNameWaLLetxxxxxxxxxxxxxxxxxxxxxxxxxxxx".into(),
        );
        let result = resolve_splits(&rules, &recipients(), 10.0, 6, &params).unwrap();
        assert_eq!(
            result[0].recipient,
            "AliasNameWaLLetxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
        );
    }

    // ── Decimal conversion ──

    #[test]
    fn resolve_decimals_6() {
        let rules = vec![SplitRule {
            recipient: "vendor".into(),
            amount: Some(0.50),
            percent: None,
            memo: None,
        }];
        let result = resolve_splits(&rules, &recipients(), 10.0, 6, &no_params()).unwrap();
        assert_eq!(result[0].amount, 500_000);
    }

    #[test]
    fn resolve_decimals_9() {
        let rules = vec![SplitRule {
            recipient: "vendor".into(),
            amount: Some(0.50),
            percent: None,
            memo: None,
        }];
        let result = resolve_splits(&rules, &recipients(), 10.0, 9, &no_params()).unwrap();
        assert_eq!(result[0].amount, 500_000_000);
    }

    // ── YAML deserialization ──

    #[test]
    fn yaml_recipients_roundtrip() {
        let yaml = r#"
vendor:
  account: "VendorWaLLetxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
  label: "Vendor"
affiliate:
  account: "${AFFILIATE_WALLET}"
"#;
        let parsed: HashMap<String, RecipientAlias> = serde_yml::from_str(yaml).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed["vendor"].account,
            "VendorWaLLetxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
        );
        assert_eq!(parsed["affiliate"].account, "${AFFILIATE_WALLET}");
        assert!(parsed["affiliate"].label.is_none());
    }

    #[test]
    fn yaml_split_rule_fixed() {
        let yaml = r#"
recipient: vendor
amount: 85.0
memo: "Vendor payout"
"#;
        let rule: SplitRule = serde_yml::from_str(yaml).unwrap();
        assert_eq!(rule.recipient, "vendor");
        assert_eq!(rule.amount, Some(85.0));
        assert!(rule.percent.is_none());
        assert_eq!(rule.memo.as_deref(), Some("Vendor payout"));
    }

    #[test]
    fn yaml_split_rule_percent() {
        let yaml = r#"
recipient: platform
percent: 2.9
"#;
        let rule: SplitRule = serde_yml::from_str(yaml).unwrap();
        assert_eq!(rule.percent, Some(2.9));
        assert!(rule.amount.is_none());
    }

    #[test]
    fn yaml_full_spec_loads() {
        let yaml = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../pdb/proxy/payment-debugger.yml"
        ))
        .unwrap();
        let spec: crate::metering::ApiSpec = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(spec.name, "payment-debugger");
        assert!(!spec.recipients.is_empty());
        // Check that an endpoint with splits loaded correctly
        let invoice = spec
            .endpoints
            .iter()
            .find(|e| e.path == "api/v1/invoices/pay")
            .unwrap();
        let splits = &invoice.metering.as_ref().unwrap().splits;
        assert_eq!(splits.len(), 3);
        assert_eq!(splits[0].recipient, "vendor");
        assert_eq!(splits[0].amount, Some(85.0));
        // Check per-tier splits
        let compute = spec
            .endpoints
            .iter()
            .find(|e| e.path == "api/v1/compute/run")
            .unwrap();
        let tiers = &compute.metering.as_ref().unwrap().dimensions[0].tiers;
        assert!(!tiers[0].splits.is_empty());
        assert!(tiers[0].splits[0].percent.is_some());
    }

    #[test]
    fn per_tier_splits_override() {
        // Tier-level splits should take precedence over metering-level splits.
        // This is a design decision, not a resolution function test — we just verify
        // the types can coexist.
        let yaml = r#"
dimensions:
  - direction: usage
    unit: requests
    scale: 1
    tiers:
      - price_usd: 0.10
        splits:
          - recipient: vendor
            percent: 20
splits:
  - recipient: platform
    percent: 5
"#;
        let metering: crate::metering::Metering = serde_yml::from_str(yaml).unwrap();
        assert_eq!(metering.splits.len(), 1);
        assert_eq!(metering.dimensions[0].tiers[0].splits.len(), 1);
    }
}
