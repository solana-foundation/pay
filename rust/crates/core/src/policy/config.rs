//! Policy definitions persisted to `~/.config/pay/policies.toml`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Current schema version. Bumped on incompatible changes.
pub const POLICIES_SCHEMA_VERSION: u32 = 1;

/// A single named spending policy.
///
/// All amounts are micro-USDC (six decimal places). An empty allowlist means
/// "any" — both lists empty is the wide-open default. When *both* lists are
/// non-empty, a request passes if it matches *either* (OR semantics).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Human-readable name; also the key in [`PoliciesFile::policies`]. The
    /// name is duplicated here so a `Policy` value carries its own identity
    /// once removed from the map.
    pub name: String,

    /// Per-transaction cap in micro-USDC. Reject if `amount > max_per_tx`.
    pub max_per_tx: u64,

    /// Daily cap in micro-USDC. Reject if `spent_today + amount > daily_cap`.
    pub daily_cap: u64,

    /// Allowlisted recipient base58 wallet pubkeys. Empty ⇒ no restriction
    /// from this list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_recipients: Vec<String>,

    /// Allowlisted request URL hosts (e.g. `api.example.com`). Empty ⇒ no
    /// restriction from this list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,

    /// Optional expiry. Reject after this timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,

    /// Hard kill switch. When true, every request is rejected.
    #[serde(default, skip_serializing_if = "is_false")]
    pub paused: bool,

    /// When this policy was created (informational).
    pub created_at: DateTime<Utc>,
}

fn is_false(b: &bool) -> bool {
    !b
}

/// Top-level on-disk shape of `policies.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoliciesFile {
    /// Schema version. Bumped on incompatible changes.
    #[serde(default = "default_version")]
    pub version: u32,

    /// Optional default policy name. Used when the user has not passed
    /// `--policy <name>` and the active account has no `policy` binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,

    /// Named policies. `BTreeMap` for deterministic on-disk order.
    #[serde(default)]
    pub policies: BTreeMap<String, Policy>,
}

impl Default for PoliciesFile {
    fn default() -> Self {
        Self {
            version: POLICIES_SCHEMA_VERSION,
            default: None,
            policies: BTreeMap::new(),
        }
    }
}

fn default_version() -> u32 {
    POLICIES_SCHEMA_VERSION
}

impl PoliciesFile {
    /// Look up a policy by name.
    pub fn get(&self, name: &str) -> Option<&Policy> {
        self.policies.get(name)
    }

    /// Insert or overwrite a policy.
    pub fn upsert(&mut self, policy: Policy) {
        self.policies.insert(policy.name.clone(), policy);
    }

    /// Remove a policy. Also clears the `default` field if it pointed at
    /// the removed name.
    pub fn remove(&mut self, name: &str) -> Option<Policy> {
        let removed = self.policies.remove(name);
        if self.default.as_deref() == Some(name) {
            self.default = None;
        }
        removed
    }

    /// Set the default policy. Errors via `Option::None` if no policy by
    /// that name exists; caller decides how to surface that.
    pub fn set_default(&mut self, name: &str) -> Option<()> {
        if !self.policies.contains_key(name) {
            return None;
        }
        self.default = Some(name.to_string());
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_policy(name: &str) -> Policy {
        Policy {
            name: name.to_string(),
            max_per_tx: 100_000,
            daily_cap: 1_000_000,
            allowed_recipients: vec![],
            allowed_origins: vec!["api.example.com".to_string()],
            expires_at: None,
            paused: false,
            created_at: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        }
    }

    #[test]
    fn toml_roundtrip_minimal() {
        let mut file = PoliciesFile::default();
        file.upsert(sample_policy("test"));
        let serialized = toml::to_string(&file).unwrap();
        let deserialized: PoliciesFile = toml::from_str(&serialized).unwrap();
        assert_eq!(file.policies, deserialized.policies);
        assert_eq!(file.version, deserialized.version);
    }

    #[test]
    fn toml_skips_empty_optional_fields() {
        let mut file = PoliciesFile::default();
        file.upsert(sample_policy("test"));
        let serialized = toml::to_string(&file).unwrap();
        assert!(!serialized.contains("allowed_recipients"));
        assert!(!serialized.contains("expires_at"));
        assert!(!serialized.contains("paused"));
    }

    #[test]
    fn remove_clears_default_pointer() {
        let mut file = PoliciesFile::default();
        file.upsert(sample_policy("a"));
        file.upsert(sample_policy("b"));
        file.set_default("a").unwrap();
        file.remove("a");
        assert!(file.default.is_none());
    }

    #[test]
    fn set_default_rejects_missing_name() {
        let mut file = PoliciesFile::default();
        assert!(file.set_default("missing").is_none());
        assert!(file.default.is_none());
    }
}
