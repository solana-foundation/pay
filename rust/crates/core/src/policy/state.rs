//! Per-policy spending state persisted to `~/.config/pay/policy-state.json`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Current schema version.
pub const STATE_SCHEMA_VERSION: u32 = 1;

/// Top-level state file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyState {
    #[serde(default = "default_version")]
    pub version: u32,

    /// Per-policy rolling state, keyed by policy name.
    #[serde(default)]
    pub per_policy: BTreeMap<String, PerPolicyState>,
}

impl Default for PolicyState {
    fn default() -> Self {
        Self {
            version: STATE_SCHEMA_VERSION,
            per_policy: BTreeMap::new(),
        }
    }
}

fn default_version() -> u32 {
    STATE_SCHEMA_VERSION
}

/// Rolling spend tracker for a single named policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerPolicyState {
    /// Micro-USDC spent within the current 24-hour window.
    #[serde(default)]
    pub spent_today: u64,

    /// When the current 24-hour window started. Reset to "now" each time
    /// the window rolls over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day_reset_ts: Option<DateTime<Utc>>,

    /// Last successful payment timestamp (informational).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_paid_at: Option<DateTime<Utc>>,
}

impl PolicyState {
    /// Get-or-insert the per-policy slot for `name`.
    pub fn entry_mut(&mut self, name: &str) -> &mut PerPolicyState {
        self.per_policy.entry(name.to_string()).or_default()
    }

    /// Drop a policy's state (called when the policy is deleted).
    pub fn forget(&mut self, name: &str) {
        self.per_policy.remove(name);
    }
}
